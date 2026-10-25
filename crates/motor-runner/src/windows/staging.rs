//! Replacement coverage for the host-side link staging this path no longer does.
//!
//! The Windows runner prepares a job by shipping bare Git repositories and
//! crate archives to the VM, where cargo checks dependencies out with
//! `core.symlinks=false`. A tracked symlink therefore never reaches a host
//! filesystem: the repositories below are object stores with no working tree,
//! and the checkout that reads them runs inside the guest. These tests pin
//! that, including a link whose target is outside the repository.

#[cfg(all(test, unix))]
mod tests {
    use crate::windows::prep as prep_module;
    use cortexkit_test_support::ScratchDir;
    use std::{
        collections::BTreeMap,
        fs,
        io::{self, Read},
        os::unix::fs::symlink,
        path::{Path, PathBuf},
        process::Command,
    };

    fn git(root: &Path, args: &[&str]) -> String {
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
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn commit(root: &Path, message: &str) {
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                message,
            ],
        );
    }

    /// A dependency whose tracked links include one pointing outside the
    /// repository, plus the secrets that target would reach.
    fn dependency_with_links(scratch: &Path) -> (PathBuf, String, String) {
        let dependency = scratch.join("dependency");
        fs::create_dir_all(dependency.join("src")).unwrap();
        fs::write(
            dependency.join("Cargo.toml"),
            "[package]\nname='linked-dependency'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(dependency.join("src/lib.rs"), "").unwrap();
        fs::write(dependency.join("README.md"), "dependency readme").unwrap();
        fs::write(scratch.join("secret"), "outside contents must not travel").unwrap();
        fs::create_dir(scratch.join("outside-dir")).unwrap();
        fs::write(
            scratch.join("outside-dir/secret"),
            "outside directory contents",
        )
        .unwrap();
        for (name, target) in [
            ("AGENTS.md", "README.md"),
            ("escape", "../../../../../../secret"),
            ("directory-link", "../../../../../../outside-dir"),
            ("dangling", "../../../../../../missing"),
        ] {
            symlink(target, dependency.join(name)).unwrap();
        }
        commit(&dependency, "dependency with tracked links");
        let rev = git(&dependency, &["rev-parse", "HEAD"]);
        (
            dependency,
            rev,
            "https://example.invalid/linked-dependency".into(),
        )
    }

    /// The bare repository the host ships: git's own object store, which has
    /// no working tree and so cannot contain a link.
    fn bare_mirror(dependency: &Path, target: &Path) -> io::Result<()> {
        let output = Command::new("git")
            .args(["clone", "-q", "--bare"])
            .arg(dependency)
            .arg(target)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    fn symlinks_below(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                if kind.is_symlink() {
                    found.push(entry.path());
                } else if kind.is_dir() {
                    pending.push(entry.path());
                }
            }
        }
        found
    }

    #[test]
    fn shipped_git_mirror_holds_no_working_tree_and_so_no_link() {
        // What the host stages for the guest is the bare repository, so the
        // dependency's links never reach a host filesystem at all.
        let scratch = ScratchDir::new("windows-mirror-links");
        let (dependency, _rev, _url) = dependency_with_links(scratch.path());
        assert_eq!(
            symlinks_below(&dependency).len(),
            4,
            "the fixture must really carry the links this test is about"
        );
        let mirror = scratch.path().join("mirror.git");
        bare_mirror(&dependency, &mirror).unwrap();
        assert_eq!(
            symlinks_below(&mirror),
            Vec::<PathBuf>::new(),
            "a dependency mirror must be an object store with no links"
        );
    }

    #[test]
    fn guest_checkout_stages_tracked_links_as_target_text() {
        // The guest checks out with `core.symlinks=false`, the setting the
        // prepared gitconfig pins. A link arrives as a file holding its
        // target text, so nothing in the checkout can resolve anywhere.
        let scratch = ScratchDir::new("windows-guest-checkout");
        let (dependency, rev, _url) = dependency_with_links(scratch.path());
        let mirror = scratch.path().join("mirror.git");
        bare_mirror(&dependency, &mirror).unwrap();
        let checkout = scratch.path().join("checkout");
        let output = Command::new("git")
            .args(["-c", "core.symlinks=false", "clone", "-q"])
            .arg(&mirror)
            .arg(&checkout)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), rev);
        assert_eq!(
            symlinks_below(&checkout),
            Vec::<PathBuf>::new(),
            "the guest checkout must contain no link"
        );
        for (name, text) in [
            ("AGENTS.md", "README.md"),
            ("escape", "../../../../../../secret"),
            ("directory-link", "../../../../../../outside-dir"),
            ("dangling", "../../../../../../missing"),
        ] {
            assert!(fs::symlink_metadata(checkout.join(name)).unwrap().is_file());
            assert_eq!(fs::read_to_string(checkout.join(name)).unwrap(), text);
        }
    }

    #[test]
    fn staged_tree_refuses_a_link_rather_than_archiving_it() {
        // If one ever appeared on the host it must stop preparation, not
        // travel to the guest where it could resolve.
        let scratch = ScratchDir::new("windows-staged-link");
        let staging = scratch.path().join("staging");
        let checkout = staging.join("cargo-home/git/checkouts/dependency/rev");
        fs::create_dir_all(&checkout).unwrap();
        fs::create_dir_all(staging.join("m/repository")).unwrap();
        fs::write(checkout.join("README.md"), "readme").unwrap();
        symlink("README.md", checkout.join("AGENTS.md")).unwrap();
        let error = prep_module::build_tar(&staging).unwrap_err().to_string();
        assert_eq!(
            error,
            "link in staged tree: cargo-home/git/checkouts/dependency/rev/AGENTS.md"
        );
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
        let error = prep_module::build_tar(&staging).unwrap_err().to_string();
        assert_eq!(
            error,
            "special file: cargo-home/git/checkouts/dependency/rev/fifo"
        );
    }

    /// The root workspace's manifest and lockfile, with the nested one below
    /// it and the git dependency the nested lockfile names.
    fn workspace_with_nested_git_dependency(scratch: &Path, url: &str, rev: &str) -> PathBuf {
        let repository = scratch.join("repository");
        fs::create_dir_all(repository.join("src")).unwrap();
        fs::create_dir_all(repository.join("playground/src")).unwrap();
        fs::write(
            repository.join("Cargo.toml"),
            "[workspace]\n[package]\nname='root'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(
            repository.join("Cargo.lock"),
            "version=4\n[[package]]\nname='root'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(repository.join("src/lib.rs"), "").unwrap();
        fs::write(
            repository.join("playground/Cargo.toml"),
            format!(
                "[workspace]\n[package]\nname='playground'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nlinked-dependency={{git='{url}',rev='{rev}'}}\n"
            ),
        )
        .unwrap();
        fs::write(
            repository.join("playground/Cargo.lock"),
            format!(
                "version=4\n[[package]]\nname='playground'\nversion='0.1.0'\ndependencies=['linked-dependency']\n[[package]]\nname='linked-dependency'\nversion='0.1.0'\nsource='git+{url}?rev={rev}#{rev}'\n"
            ),
        )
        .unwrap();
        fs::write(repository.join("playground/src/lib.rs"), "").unwrap();
        commit(&repository, "nested workspace");
        repository
    }

    fn test_plan(staging: &Path, repository: &Path, url: &str, mirror: &Path) -> prep_module::Plan {
        let cargo = PathBuf::from(env!("CARGO"));
        prep_module::Plan {
            staging: staging.to_path_buf(),
            snapshots: vec![prep_module::Snapshot {
                mirror: repository.join(".git"),
                commit: git(repository, &["rev-parse", "HEAD"]),
                destination: "/repository".into(),
            }],
            routes: BTreeMap::from([(url.to_owned(), mirror.to_path_buf())]),
            workspace: "/repository".into(),
            tree_hash: git(repository, &["rev-parse", "HEAD^{tree}"]),
            cargo: cargo.clone(),
            rustup: std::env::var_os("RUSTUP_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup"))
                })
                .expect("test toolchain has a Rustup home"),
            toolchain_bin: cargo.parent().unwrap().to_path_buf(),
            toolchain_source: PathBuf::new(),
        }
    }

    #[test]
    fn nested_workspace_git_dependency_reaches_offline_cargo() {
        // The nested lockfile is not the root one, so its git dependency has to
        // be fetched on its own; with the mirror shipped under cargo-home the
        // offline metadata command then resolves it inside the guest.
        let scratch = ScratchDir::new("windows-nested-offline");
        let (dependency, rev, url) = dependency_with_links(scratch.path());
        let repository = workspace_with_nested_git_dependency(scratch.path(), &url, &rev);
        let mirror = scratch.path().join("mirror.git");
        bare_mirror(&dependency, &mirror).unwrap();

        let staging = scratch.path().join("staging");
        fs::create_dir_all(&staging).unwrap();
        let shipped = staging.join("cargo-home/git-mirrors/0");
        fs::create_dir_all(shipped.parent().unwrap()).unwrap();
        bare_mirror(&dependency, &shipped).unwrap();
        let plan = test_plan(&staging, &repository, &url, &shipped);
        let plan_path = staging.join("prepare.json");
        super::super::atomic_json(&plan_path, &plan).unwrap();
        prep_module::execute(&plan_path).unwrap();

        // The guest's git configuration, as the prepared cargo home carries
        // it; pinned off so the checkout below stages links as target text.
        fs::create_dir_all(staging.join("cargo-home")).unwrap();
        fs::write(
            staging.join("cargo-home/gitconfig"),
            b"[core]\n\tsymlinks = false\n",
        )
        .unwrap();
        let output = prep_module::cargo_command(
            &plan,
            &staging.join("m/repository/playground/Cargo.toml"),
            &["metadata", "--locked", "--offline", "--format-version", "1"],
        )
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            metadata["packages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|package| package["name"] == "linked-dependency"),
            "the nested workspace's git dependency is missing from cargo metadata"
        );
        // The checkout cargo made for it lives inside the staging tree and
        // holds no link, including for the outside target.
        let checkouts = staging.join("cargo-home/git/checkouts");
        if checkouts.exists() {
            assert_eq!(symlinks_below(&checkouts), Vec::<PathBuf>::new());
        }
    }

    #[test]
    fn no_archive_member_carries_the_outside_target_contents() {
        // The staged tree the host archives holds mirror bytes only, so no
        // member can contain what the links pointed at on the source machine.
        let scratch = ScratchDir::new("windows-archive-links");
        let (dependency, _rev, _url) = dependency_with_links(scratch.path());
        let staging = scratch.path().join("staging");
        let mirror_target = staging.join("cargo-home/git-mirrors/0");
        fs::create_dir_all(mirror_target.parent().unwrap()).unwrap();
        bare_mirror(&dependency, &mirror_target).unwrap();
        fs::create_dir_all(staging.join("m/repository")).unwrap();
        prep_module::build_tar(&staging).unwrap();
        let mut members = BTreeMap::new();
        for entry in tar::Archive::new(fs::File::open(staging.join("tree.tar")).unwrap())
            .entries()
            .unwrap()
        {
            let mut entry = entry.unwrap();
            assert!(
                !entry.header().entry_type().is_symlink(),
                "archive entry is a link"
            );
            if entry.header().entry_type().is_file() {
                let path = entry.path().unwrap().to_string_lossy().into_owned();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                members.insert(path, bytes);
            }
        }
        assert!(
            members
                .keys()
                .any(|path| path.starts_with("cargo-home/git-mirrors/0/")),
            "the shipped mirror is missing from the archive"
        );
        for (path, bytes) in &members {
            for secret in [
                b"outside contents must not travel".as_slice(),
                b"outside directory contents".as_slice(),
            ] {
                assert!(
                    !bytes.windows(secret.len()).any(|part| part == secret),
                    "leaked into {path}"
                );
            }
        }
    }
}
