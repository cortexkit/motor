use super::*;
#[cfg(any(windows, test))]
use std::{fs::OpenOptions, io::Write};

/// Match Git for Windows with core.symlinks=false without opening the target.
pub(super) fn link_text(path: &Path) -> io::Result<Vec<u8>> {
    Ok(fs::read_link(path)?.as_os_str().as_encoded_bytes().to_vec())
}

#[cfg(any(windows, test))]
pub(super) fn copy_tree(source: &Path, target: &Path, staging: &Path) -> io::Result<()> {
    copy(source, target, staging)
}

#[cfg(any(windows, test))]
fn copy(source: &Path, target: &Path, staging: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_symlink() {
        let text = link_text(source)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)?
            .write_all(&text)?;
    } else {
        // On Windows this also refuses non-symlink reparse points, including
        // junctions, before traversing directories or copying file contents.
        crate::filesystem::identity(source)?;
        if metadata.is_dir() {
            fs::create_dir_all(target)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                copy(&entry.path(), &target.join(entry.file_name()), staging)?;
            }
        } else if metadata.is_file() {
            let mut input = crate::filesystem::read_nofollow(source)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)?;
            io::copy(&mut input, &mut output)?;
        } else {
            return Err(invalid(format!(
                "special file: {}",
                target.strip_prefix(staging).map_err(invalid)?.display()
            )));
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::{collections::BTreeMap, io::Read, os::unix::fs::symlink};

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn git_dependency_links_and_outside_targets_are_staged_as_text() {
        let scratch = ScratchDir::new("windows-git-links");
        let repo = scratch.path().join("dependency");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("README.md"), b"inside contents").unwrap();
        fs::write(
            scratch.path().join("secret"),
            b"outside contents must not travel",
        )
        .unwrap();
        fs::create_dir(scratch.path().join("outside-dir")).unwrap();
        fs::write(
            scratch.path().join("outside-dir/secret"),
            b"outside directory contents",
        )
        .unwrap();
        for (name, target) in [
            ("AGENTS.md", "README.md"),
            ("escape", "../../../../../../secret"),
            ("directory-link", "../../../../../../outside-dir"),
            ("dangling", "../../../../../../missing"),
        ] {
            symlink(target, repo.join(name)).unwrap();
        }
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "dependency with tracked links",
            ],
        );
        let staging = scratch.path().join("staging");
        fs::create_dir_all(staging.join("m/repository")).unwrap();
        let checkout = staging.join("cargo-home/git/checkouts/dependency/rev");
        fs::create_dir_all(checkout.parent().unwrap()).unwrap();
        let output = Command::new("git")
            .args(["-c", "core.symlinks=true", "clone", "-q"])
            .arg(&repo)
            .arg(&checkout)
            .output()
            .unwrap();
        assert!(output.status.success());
        // The relative targets resolve to real files outside the entire staged
        // Cargo tree, so no archive member may contain those external bytes.
        assert_eq!(
            fs::read(checkout.join("escape")).unwrap(),
            b"outside contents must not travel"
        );

        let ally = scratch.path().join("ally");
        for prefix in ["git/checkouts/dependency/rev", "registry/index/local"] {
            let target = ally.join(prefix);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            copy_tree(&checkout, &target, &ally).unwrap();
            for (name, text) in [
                ("AGENTS.md", "README.md"),
                ("escape", "../../../../../../secret"),
                ("directory-link", "../../../../../../outside-dir"),
                ("dangling", "../../../../../../missing"),
            ] {
                assert!(fs::symlink_metadata(target.join(name)).unwrap().is_file());
                assert_eq!(fs::read(target.join(name)).unwrap(), text.as_bytes());
            }
        }
        super::super::prep::build_tar(&staging).unwrap();
        let mut found = BTreeMap::new();
        for entry in tar::Archive::new(fs::File::open(staging.join("tree.tar")).unwrap())
            .entries()
            .unwrap()
        {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            if entry.header().entry_type().is_file() {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                assert!(
                    ![
                        b"outside contents must not travel".as_slice(),
                        b"outside directory contents".as_slice()
                    ]
                    .iter()
                    .any(|secret| bytes.windows(secret.len()).any(|part| part == *secret)),
                    "leaked {path}"
                );
                found.insert(path, bytes);
            } else {
                assert!(entry.header().entry_type().is_dir());
            }
        }
        for (name, text) in [
            ("AGENTS.md", "README.md"),
            ("escape", "../../../../../../secret"),
            ("directory-link", "../../../../../../outside-dir"),
            ("dangling", "../../../../../../missing"),
        ] {
            assert_eq!(
                found[&format!("cargo-home/git/checkouts/dependency/rev/{name}")],
                text.as_bytes()
            );
        }
    }

    #[test]
    fn dependency_staging_refuses_special_files_with_a_relative_path() {
        let scratch = ScratchDir::new("windows-special-file");
        let staging = scratch.path().join("staging");
        let dependency = staging.join("cargo-home/git/checkouts/dependency/rev");
        fs::create_dir_all(&dependency).unwrap();
        fs::create_dir_all(staging.join("m/repository")).unwrap();
        let path =
            std::ffi::CString::new(dependency.join("fifo").as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let error = super::super::prep::build_tar(&staging)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "special file: cargo-home/git/checkouts/dependency/rev/fifo"
        );
        let error = copy_tree(&dependency, &scratch.path().join("ally"), scratch.path())
            .unwrap_err()
            .to_string();
        assert_eq!(error, "special file: ally/fifo");
    }
}
