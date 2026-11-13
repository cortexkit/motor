use super::*;

/// Ordinary snapshot directories cannot be repositories: caller-side nested
/// repositories are excluded or represented by gitlinks. Remove job-created Git
/// metadata only there, leaving gitlinks and ignored build directories alone.
pub(super) fn remove_nested_git_metadata(
    objects: &Path,
    workspace: &Path,
    tree: &str,
) -> io::Result<bool> {
    let directories = git_bytes(objects, &["ls-tree", "-drz", "--full-tree", tree])?;
    let mut removed = false;
    for entry in directories.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let tab = entry
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| invalid("missing snapshot directory path"))?;
        if !entry[..tab].starts_with(b"040000 tree ") {
            continue;
        }
        let relative = change_path(&entry[tab + 1..]);
        // The runner owns root target, even if an older commit contains it.
        if relative.starts_with("target") {
            continue;
        }
        let mut directory = workspace.to_path_buf();
        let mut exists = true;
        for component in relative.components() {
            if !matches!(component, std::path::Component::Normal(_)) {
                return Err(invalid("invalid snapshot directory path"));
            }
            directory.push(component);
            // Do not follow a job-created symlink out of the checkout, even
            // when the snapshot expects an ordinary directory at that path.
            match fs::symlink_metadata(&directory) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    exists = false;
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    exists = false;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if !exists {
            continue;
        }
        let metadata_path = directory.join(".git");
        match fs::symlink_metadata(&metadata_path) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    fs::remove_dir_all(&metadata_path)?;
                } else {
                    fs::remove_file(&metadata_path)?;
                }
                removed = true;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "-q"]).unwrap();
    }

    #[test]
    fn reset_preserves_gitlinks_ignored_repositories_and_root_target() {
        let scratch = crate::test_helpers::CanonicalScratchDir::new("nested-git-reset");
        let workspace = scratch.join("workspace");
        repository(&workspace);
        fs::write(workspace.join(".gitignore"), "cache/\n").unwrap();
        for name in ["source/deep\nname", "target"] {
            fs::create_dir_all(workspace.join(name)).unwrap();
            fs::write(workspace.join(name).join("input"), "source").unwrap();
        }
        git(&workspace, &["add", "."]).unwrap();
        git(
            &workspace,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "source",
            ],
        )
        .unwrap();
        let head = git(&workspace, &["rev-parse", "HEAD"]).unwrap();
        git(
            &workspace,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{head},submodule"),
            ],
        )
        .unwrap();
        let tree = git(&workspace, &["write-tree"]).unwrap();
        for name in ["source/deep\nname", "target", "cache", "submodule"] {
            repository(&workspace.join(name));
        }

        assert!(remove_nested_git_metadata(&workspace, &workspace, &tree).unwrap());
        assert!(!workspace.join("source/deep\nname/.git").exists());
        for name in [".git", "target/.git", "cache/.git", "submodule/.git"] {
            assert!(workspace.join(name).is_dir(), "{name}");
        }
        assert!(!remove_nested_git_metadata(&workspace, &workspace, &tree).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn reset_never_follows_source_directory_or_git_metadata_symlinks() {
        let scratch = crate::test_helpers::CanonicalScratchDir::new("nested-git-reset-symlink");
        let workspace = scratch.join("workspace");
        repository(&workspace);
        fs::create_dir_all(workspace.join("source/deep")).unwrap();
        fs::write(workspace.join("source/deep/input"), "source").unwrap();
        git(&workspace, &["add", "."]).unwrap();
        let tree = git(&workspace, &["write-tree"]).unwrap();
        let outside = scratch.join("outside");
        fs::rename(workspace.join("source"), &outside).unwrap();
        repository(&outside);
        repository(&outside.join("deep"));
        std::os::unix::fs::symlink(&outside, workspace.join("source")).unwrap();
        assert!(!remove_nested_git_metadata(&workspace, &workspace, &tree).unwrap());
        assert!(outside.join(".git").is_dir());
        assert!(outside.join("deep/.git").is_dir());

        fs::remove_file(workspace.join("source")).unwrap();
        fs::create_dir_all(workspace.join("source/deep")).unwrap();
        std::os::unix::fs::symlink(outside.join(".git"), workspace.join("source/.git")).unwrap();
        assert!(remove_nested_git_metadata(&workspace, &workspace, &tree).unwrap());
        assert!(outside.join(".git").is_dir());
        assert!(fs::symlink_metadata(workspace.join("source/.git")).is_err());
    }
}
