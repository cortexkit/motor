use super::*;
use cortexkit_test_support::ScratchDir;

use motor_runner::image_source_tree_already_readable as runner_readable;

#[test]
fn account_copies_have_full_private_extents_even_when_source_is_reflinked() {
    let scratch = ScratchDir::new("motor-image-physical-copy");
    let source = scratch.join("source");
    let mut file = File::create(&source).unwrap();
    file.write_all(&vec![0u8; 4 * 1024 * 1024]).unwrap();
    file.sync_all().unwrap();
    let clone = scratch.join("source-clone");
    let mut cloned = File::create(&clone).unwrap();
    fs::reflink(&File::open(&source).unwrap(), &cloned).unwrap();
    assert_eq!(
        super::super::job::image_copy_exclusive_bytes(&cloned).unwrap(),
        0
    );
    assert!(require_physical_copy(&cloned).is_err());
    cloned = File::open(&clone).unwrap();
    for name in ["account-a", "account-b"] {
        let path = scratch.join(name);
        physical_copy(&mut cloned, &path).unwrap();
        let mut copy = File::open(&path).unwrap();
        assert_eq!(copy.metadata().unwrap().len(), 4 * 1024 * 1024);
        assert_eq!(
            super::super::job::image_copy_exclusive_bytes(&copy).unwrap(),
            4 * 1024 * 1024
        );
        assert_eq!(digest(&mut copy).unwrap(), digest(&mut cloned).unwrap());
        assert_ne!(
            copy.metadata().unwrap().ino(),
            cloned.metadata().unwrap().ino()
        );
    }
    std::fs::hard_link(scratch.join("account-a"), scratch.join("linked-account")).unwrap();
    assert!(require_physical_copy(&File::open(scratch.join("account-a")).unwrap()).is_err());
}

#[test]
fn publication_acl_provisioner_satisfies_the_runners_actual_startup_check() {
    let scratch = ScratchDir::new("motor-image-source-acls");
    let uid = unsafe { libc::geteuid() } + 1;
    for name in ["registry", "git"] {
        let root = scratch.join(name);
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/source"), b"crate").unwrap();
        assert!(!runner_readable(&root, uid, true));
        provision_tree(&root, uid).unwrap();
        assert!(runner_readable(&root, uid, true));
        // A newly fetched source must inherit the same read/search grant.
        std::fs::create_dir(root.join("new")).unwrap();
        std::fs::write(root.join("new/entry"), b"new source").unwrap();
        assert!(runner_readable(&root, uid, true));
    }
}

#[test]
fn image_requests_are_identity_only_and_reject_paths_and_unknown_fields() {
    for op in [
        "inspect_job_images",
        "publish_job_images",
        "prepare_job_images",
        "remove_job_images",
    ] {
        let mut value = serde_json::json!({"op":op,"generation":"gen-1"});
        if op == "prepare_job_images" {
            value["account"] = serde_json::json!("account-a");
        }
        assert!(serde_json::from_value::<crate::Request>(value.clone()).is_ok());
        value["path"] = serde_json::json!("/elsewhere");
        assert!(serde_json::from_value::<crate::Request>(value).is_err());
    }
    for id in ["../gen", "/gen", "", "gen.name"] {
        assert!(Operation::Publish(id.into()).validate().is_err());
        assert!(Operation::Prepare {
            generation: "gen".into(),
            account: id.into()
        }
        .validate()
        .is_err());
    }
}

#[cfg(feature = "linux-root-tests")]
mod root {
    use super::*;

    fn root_scratch(name: &str) -> ScratchDir {
        assert_eq!(
            unsafe { libc::geteuid() },
            0,
            "run outside remote jobs as root"
        );
        ScratchDir::new(name)
    }

    fn ext4(path: &Path, tree: &Path) {
        File::create(path)
            .unwrap()
            .set_len(32 * 1024 * 1024)
            .unwrap();
        assert!(Command::new("/usr/sbin/mkfs.ext4")
            .args(["-q", "-F", "-d"])
            .arg(tree)
            .arg(path)
            .status()
            .unwrap()
            .success());
    }

    fn stage(base: &Path) -> (PathBuf, PathBuf, Manifest) {
        let staging = base.join("staging");
        let root = base.join("images");
        directory(&staging.join("gen-1")).unwrap();
        directory(&root).unwrap();
        let source = staging.join("gen-1");
        let tree = base.join("root-tree");
        directory(&tree.join("etc")).unwrap();
        std::fs::write(
            tree.join("etc/passwd"),
            "root:x:0:0::/:/bin/sh\nmotor-job:x:12345:12345::/:/bin/sh\n",
        )
        .unwrap();
        ext4(&source.join("rootfs.ext4"), &tree);
        let tools = base.join("tool-tree");
        for name in ["registry", "git"] {
            directory(
                &tools
                    .join("usr/local/lib/motor/cargo")
                    .join(name)
                    .join("nested"),
            )
            .unwrap();
            std::fs::write(
                tools
                    .join("usr/local/lib/motor/cargo")
                    .join(name)
                    .join("nested/source"),
                b"crate",
            )
            .unwrap();
        }
        ext4(&source.join("toolchain.ext4"), &tools);
        std::fs::write(source.join("kernel"), b"pinned kernel").unwrap();
        let hash = |name| digest(&mut File::open(source.join(name)).unwrap()).unwrap();
        let pinned = Manifest {
            kernel: hash("kernel"),
            rootfs: hash("rootfs.ext4"),
            toolchain: hash("toolchain.ext4"),
            hello: serde_json::json!({"type":"hello"}),
            guest_init: serde_json::json!({}),
        };
        std::fs::write(
            source.join("manifest.json"),
            serde_json::to_vec(&pinned).unwrap(),
        )
        .unwrap();
        (staging, root, pinned)
    }

    #[test]
    fn isolated_publication_verifies_pins_and_seals_acl_ready_physical_account_images() {
        let scratch = root_scratch("motor-image-publication");
        let (staging, root, pinned) = stage(scratch.path());
        publish(&staging, &root, "gen-1", provision).unwrap();
        let image = root.join("gen-1");
        let sealed = manifest(&image).unwrap();
        assert_eq!(sealed.kernel, pinned.kernel);
        assert_eq!(sealed.rootfs, pinned.rootfs);
        assert_ne!(sealed.toolchain.sha256, pinned.toolchain.sha256);
        for account in ["account-a", "account-b"] {
            prepare(&root, "gen-1", account).unwrap();
            let path = image.join("accounts").join(account).join("toolchain.ext4");
            let file = verify(&path, &sealed.toolchain).unwrap();
            assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o444);
            assert_eq!(
                super::super::super::job::image_copy_exclusive_bytes(&file).unwrap(),
                sealed.toolchain.bytes
            );
            let mounted = Mount::open(&path, &scratch.join("check-mount"), true).unwrap();
            for name in ["registry", "git"] {
                assert!(runner_readable(
                    &mounted.0.join("usr/local/lib/motor/cargo").join(name),
                    12345,
                    true
                ));
            }
            mounted.close().unwrap();
        }
        let shared = image.join("accounts/account-shared");
        directory(&shared).unwrap();
        let file = File::create(shared.join("toolchain.ext4")).unwrap();
        fs::reflink(&File::open(image.join("toolchain.ext4")).unwrap(), &file).unwrap();
        seal(&shared.join("toolchain.ext4")).unwrap();
        assert!(prepare(&root, "gen-1", "account-shared").is_err());
        let linked = image.join("accounts/account-linked");
        directory(&linked).unwrap();
        std::fs::hard_link(
            image.join("accounts/account-a/toolchain.ext4"),
            linked.join("toolchain.ext4"),
        )
        .unwrap();
        assert!(prepare(&root, "gen-1", "account-linked").is_err());
        // Publication is idempotent, but an identity cannot be overwritten.
        publish(&staging, &root, "gen-1", provision).unwrap();
        let mut changed = pinned;
        changed.kernel.sha256 = "b".repeat(64);
        std::fs::write(
            staging.join("gen-1/manifest.json"),
            serde_json::to_vec(&changed).unwrap(),
        )
        .unwrap();
        assert!(publish(&staging, &root, "gen-1", provision).is_err());
        assert_eq!(manifest(&image).unwrap(), sealed);
    }

    #[test]
    fn isolated_publication_refuses_every_artifact_hash_mismatch_without_visible_generation() {
        for name in ["kernel", "rootfs.ext4", "toolchain.ext4"] {
            let scratch = root_scratch("motor-image-pin-mismatch");
            let (staging, root, _) = stage(scratch.path());
            let path = staging.join("gen-1").join(name);
            let mut file = OpenOptions::new().write(true).open(path).unwrap();
            file.write_all(b"corrupt").unwrap();
            file.sync_all().unwrap();
            assert!(publish(&staging, &root, "gen-1", provision)
                .unwrap_err()
                .0
                .contains("hash mismatch"));
            assert!(!root.join("gen-1").exists());
        }
    }
}
