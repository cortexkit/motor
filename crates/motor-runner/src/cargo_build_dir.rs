//! Linux-only maintenance for Cargo's per-workspace intermediate folders.
//! Each folder also holds registry and Git dependency outputs. Keep it if any
//! dependency-information (.d) file names a relative source or a source under
//! the job's workspace or a declared sibling. Unknown folders are not proven
//! foreign and are kept.
use crate::{caller_path as paths, Config};
use motor_protocol::Run;
use std::{
    ffi::{CString, OsStr, OsString},
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Path, PathBuf},
};

const MARKER: &str = ".motor-build-dir-v1";

pub(crate) fn prepare(config: &Config, request: &Run, target: &Path) {
    let fixed = paths::fixed_paths(config, request).ok().flatten();
    let workspace = paths::run(request).workspace.checkout().caller;
    let roots: Vec<_> = std::iter::once(Path::new(workspace.text()))
        .chain(
            request
                .siblings
                .iter()
                .map(|s| Path::new(paths::sibling(s).caller.text())),
        )
        .chain(fixed.iter().map(|fixed| fixed.checkout.as_path()))
        .chain(
            fixed
                .iter()
                .flat_map(|fixed| fixed.siblings.iter().map(PathBuf::as_path)),
        )
        .collect();
    let result = maintain(target, &roots, |path| {
        if !config.boat_profile {
            crate::isolation::reclaim_runtime(path)?;
        }
        Ok(())
    });
    // Target contents are job-controlled. Bad permissions, malformed trees or
    // bounded-walker limits must not prevent the next job from starting.
    if let Err(error) = result {
        tracing::warn!(path = %target.display(), %error, "Cargo build-folder maintenance failed");
    }
}

fn absent_or_link(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
}

fn directory(parent: &File, name: &OsStr) -> io::Result<Option<File>> {
    match paths::owner::directory_at(parent, Path::new(name)) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if absent_or_link(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn names(directory: &File) -> io::Result<Vec<OsString>> {
    let mut names = fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

fn file(parent: &File, name: &OsStr, flags: i32) -> io::Result<Option<File>> {
    let name = CString::new(name.as_bytes())?;
    // Nonblocking open also refuses to hang on a job-created FIFO. Inspect the
    // pinned inode before reading; O_NOFOLLOW alone does not reject devices.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o644,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return if absent_or_link(&error) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let file = unsafe { File::from_raw_fd(fd) };
    Ok(file.metadata()?.is_file().then_some(file))
}

fn profiles(root: &File) -> io::Result<Vec<(PathBuf, File)>> {
    let mut profiles = Vec::new();
    for name in names(root)? {
        if name == "bd" {
            continue;
        }
        let Some(child) = directory(root, &name)? else {
            continue;
        };
        if is_profile(&child)? {
            profiles.push((PathBuf::from(name), child));
        } else {
            for profile in names(&child)? {
                if let Some(dir) = directory(&child, &profile)? {
                    if is_profile(&dir)? {
                        profiles.push((PathBuf::from(&name).join(profile), dir));
                    }
                }
            }
        }
    }
    Ok(profiles)
}

fn is_profile(directory: &File) -> io::Result<bool> {
    Ok(self::directory(directory, OsStr::new("deps"))?.is_some()
        || self::directory(directory, OsStr::new(".fingerprint"))?.is_some())
}

fn remove(
    target: &Path,
    relative: &Path,
    parent: &File,
    name: &OsStr,
    before_remove: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    // Only children of this job's pinned target are removed, never a mapped
    // workspace root, so no workspace needs moving to its private trash first.
    // Use the existing bounded, descriptor-relative, same-device deletion
    // helper rather than walking pathnames.
    before_remove(&target.join(relative))?;
    crate::boat_quota::walk::remove_private_at(parent, name)
}

fn maintain(
    target: &Path,
    roots: &[&Path],
    mut before_remove: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let root = match paths::owner::directory(target) {
        Ok(root) => root,
        Err(error) if absent_or_link(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    if file(&root, OsStr::new(MARKER), libc::O_RDONLY)?.is_none() {
        // Cargo stops using these intermediates after the layout switch.
        // Leaving them behind would roughly double each warm target's disk use.
        for (profile, dir) in profiles(&root)? {
            // Leave a profile sentinel until last so partial deletions remain
            // discoverable when the migration retries after an error.
            for name in ["build", "incremental", "deps", ".fingerprint"] {
                if directory(&dir, OsStr::new(name))?.is_some() {
                    remove(
                        target,
                        &profile.join(name),
                        &dir,
                        OsStr::new(name),
                        &mut before_remove,
                    )?;
                }
            }
        }
        // A failed deletion returns before this point so the next job retries.
        file(
            &root,
            OsStr::new(MARKER),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )?
        .ok_or_else(|| io::Error::other("Cargo migration marker is not a regular file"))?;
    }
    cleanup(target, &root, roots, &mut before_remove)
}

fn hex(name: &OsStr, length: usize) -> bool {
    let name = name.as_bytes();
    name.len() == length && name.iter().all(u8::is_ascii_hexdigit)
}

fn cleanup(
    target: &Path,
    root: &File,
    roots: &[&Path],
    before_remove: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let Some(bd) = directory(root, OsStr::new("bd"))? else {
        return Ok(());
    };
    for prefix in names(&bd)?.into_iter().filter(|name| hex(name, 2)) {
        let Some(parent) = directory(&bd, &prefix)? else {
            continue;
        };
        for hash in names(&parent)?.into_iter().filter(|name| hex(name, 14)) {
            let Some(folder) = directory(&parent, &hash)? else {
                continue;
            };
            if matches!(dependency(&folder, roots)?, Dependency::Foreign) {
                remove(
                    target,
                    &Path::new("bd").join(&prefix).join(&hash),
                    &parent,
                    &hash,
                    before_remove,
                )?;
            }
        }
    }
    Ok(())
}

enum Dependency {
    Checkout,
    Foreign,
    Unidentified,
}

/// Scan dep-info files until any source matches the workspace or a sibling.
/// Registry and Git dependencies can sort before local crates, so an earlier
/// foreign source does not identify the whole folder as foreign. Cargo emits
/// relative paths for workspace members; those belong to the checkout that owns
/// the build directory. Unreadable or unknown dependency information cannot
/// prove it foreign.
fn dependency(folder: &File, roots: &[&Path]) -> io::Result<Dependency> {
    let mut dependency = Dependency::Unidentified;
    let mut unknown = false;
    for (_, profile) in profiles(folder)? {
        let Some(deps) = directory(&profile, OsStr::new("deps"))? else {
            continue;
        };
        for name in names(&deps)?
            .into_iter()
            .filter(|name| Path::new(name).extension() == Some(OsStr::new("d")))
        {
            let Ok(Some(file)) = file(&deps, &name, libc::O_RDONLY) else {
                unknown = true;
                continue;
            };
            let mut bytes = Vec::new();
            if file.take(64 * 1024).read_to_end(&mut bytes).is_err() {
                unknown = true;
                continue;
            }
            if bytes.len() == 64 * 1024 {
                unknown = true;
                continue;
            }
            if let Some(path) = dependency_path(&bytes) {
                if path.is_relative() || roots.iter().any(|root| path.starts_with(root)) {
                    return Ok(Dependency::Checkout);
                }
                dependency = Dependency::Foreign;
            } else {
                unknown = true;
            }
        }
    }
    Ok(if unknown {
        Dependency::Unidentified
    } else {
        dependency
    })
}

fn dependency_path(bytes: &[u8]) -> Option<PathBuf> {
    let start = bytes.windows(2).position(|pair| pair == b": ")? + 2;
    let mut path = Vec::new();
    let mut input = bytes[start..].iter().copied().peekable();
    while let Some(byte) = input.next() {
        if byte == b'\\' && input.peek() == Some(&b' ') {
            path.push(input.next()?);
        } else if byte.is_ascii_whitespace() {
            break;
        } else {
            path.push(byte);
        }
    }
    (!path.is_empty()).then(|| PathBuf::from(OsString::from_vec(path)))
}

#[cfg(test)]
#[path = "cargo_build_dir_tests.rs"]
mod tests;

#[cfg(test)]
mod fixed_path_tests {
    use super::*;

    fn request() -> Run {
        let job: crate::storage::Job = serde_json::from_str(include_str!(
            "../test-fixtures/compatibility/job-record.json"
        ))
        .unwrap();
        let mut request = job.request.unwrap();
        request.repository_root = "/caller/main/repository".into();
        request.workspace_key = "/caller/worktrees/job".into();
        request.cwd = request.workspace_key.clone();
        request.siblings = vec![motor_protocol::SiblingSnapshot {
            canonical_path: "/caller/main/commons".into(),
            commit: "a".repeat(40),
            tree_hash: "b".repeat(40),
            base_commit: None,
        }];
        request
    }

    fn folder(target: &Path, hash: &str, source: &Path) -> PathBuf {
        let folder = target.join("bd/ab").join(hash);
        let deps = folder.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        fs::write(
            deps.join("crate.d"),
            format!("out: {}/src/lib.rs\n", source.display()),
        )
        .unwrap();
        folder
    }

    #[test]
    fn prepare_keeps_fixed_checkout_and_sibling_dep_info() {
        let scratch = cortexkit_test_support::ScratchDir::new("cargo-fixed-roots");
        let target = scratch.join("target");
        let request = request();
        let mut config = Config::new(scratch.join("state"), "fixed-roots");
        config.boat_profile = true;
        config.account_subject = Some("acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into());
        config.fixed_path_repositories = vec![request.repository_root.clone()];
        let fixed = paths::fixed_paths(&config, &request).unwrap().unwrap();
        let checkout = folder(&target, "00000000000001", &fixed.checkout);
        let sibling = folder(&target, "00000000000002", &fixed.siblings[0]);
        let caller = folder(&target, "00000000000003", Path::new(&request.workspace_key));
        let caller_sibling = folder(
            &target,
            "00000000000004",
            Path::new(&request.siblings[0].canonical_path),
        );
        let foreign = folder(&target, "00000000000005", Path::new("/unrelated/repo"));
        prepare(&config, &request, &target);
        for kept in [checkout, sibling, caller, caller_sibling] {
            assert!(kept.join("debug/deps/crate.d").is_file(), "{kept:?}");
        }
        assert!(!foreign.exists());
    }

    #[test]
    fn disabled_and_colliding_jobs_use_only_legacy_prepare_roots() {
        let scratch = cortexkit_test_support::ScratchDir::new("cargo-legacy-roots");
        let mut request = request();
        let mut config = Config::new(scratch.join("state"), "fixed-roots");
        config.boat_profile = true;
        config.account_subject = Some("acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into());
        config.fixed_path_repositories = vec!["*".into()];
        let fixed = paths::fixed_paths(&config, &request).unwrap().unwrap();
        for collision in [false, true] {
            let target = scratch.join(format!("target-{collision}"));
            if collision {
                request.siblings[0].canonical_path = "/another/repository".into();
                assert!(paths::fixed_paths(&config, &request).is_err());
            } else {
                config.fixed_path_repositories.clear();
            }
            let fixed = folder(&target, "00000000000001", &fixed.checkout);
            let caller = folder(&target, "00000000000002", Path::new(&request.workspace_key));
            let sibling = folder(
                &target,
                "00000000000003",
                Path::new(&request.siblings[0].canonical_path),
            );
            prepare(&config, &request, &target);
            assert!(!fixed.exists());
            assert!(caller.exists());
            assert!(sibling.exists());
            config.fixed_path_repositories = vec!["*".into()];
        }
    }
}
