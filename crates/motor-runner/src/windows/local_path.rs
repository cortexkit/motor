//! Resolve checkout dependency paths without opening attacker-selected host paths.
use std::{
    io,
    path::{Component, Path, PathBuf},
};

pub(super) fn resolve(directory: &Path, roots: &[PathBuf], local: &str) -> io::Result<PathBuf> {
    if local.is_empty() || local.contains([':', '\\']) || local.starts_with('/') {
        return Err(io::Error::other(
            "local dependency must be a relative checkout path",
        ));
    }
    let mut target = directory.to_path_buf();
    for component in Path::new(local).components() {
        match component {
            Component::Normal(name) => target.push(name),
            // `path = "."` is Cargo's common self dev-dependency for test
            // features; a `.` component stays in the same folder.
            Component::CurDir => (),
            Component::ParentDir if target.pop() => (),
            _ => return Err(io::Error::other("invalid local dependency component")),
        }
    }
    if !roots.iter().any(|root| target.starts_with(root)) {
        return Err(io::Error::other(
            "local dependency is outside the tracked checkouts",
        ));
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_paths_reject_network_drives_and_escape_without_filesystem_resolution() {
        let root = PathBuf::from("checkout");
        for raw in [
            "",
            "/absolute",
            "C:/drive",
            "C:relative",
            r"\\evil\share",
            r"\evil\share",
            r"foo\bar",
            "https://evil",
            "../../escape",
        ] {
            assert!(
                resolve(&root.join("project"), std::slice::from_ref(&root), raw).is_err(),
                "accepted {raw}"
            );
        }
        assert_eq!(
            resolve(
                &root.join("project"),
                std::slice::from_ref(&root),
                "../vendor/lib"
            )
            .unwrap(),
            root.join("vendor/lib")
        );
    }

    #[test]
    fn local_paths_accept_the_current_folder() {
        let root = PathBuf::from("checkout");
        let project = root.join("project");
        for (raw, expected) in [
            (".", project.clone()),
            ("./vendor/lib", project.join("vendor/lib")),
        ] {
            assert_eq!(
                resolve(&project, std::slice::from_ref(&root), raw).unwrap(),
                expected,
                "{raw}"
            );
        }
    }
}
