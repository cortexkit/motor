use std::{collections::BTreeMap, path::Path};

pub(super) fn arm64_windows(env: &mut BTreeMap<String, String>, root: &Path) {
    if !root.is_dir() {
        return;
    }
    // cc-rs needs the image's Universal C Runtime headers for ARM64 Windows
    // GNU/LLVM, not host clang or a Microsoft SDK. Keep these paths runner-owned:
    // CC_/AR_ request overrides are not admitted, and the linker must use the same
    // sysroot. Target-scoped names leave every other compiler selection alone.
    for (name, binary) in [
        ("CC_aarch64_pc_windows_gnullvm", "aarch64-w64-mingw32-clang"),
        ("AR_aarch64_pc_windows_gnullvm", "llvm-ar"),
        (
            "CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER",
            "aarch64-w64-mingw32-clang",
        ),
    ] {
        env.insert(
            name.into(),
            root.join("bin").join(binary).to_string_lossy().into_owned(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_arm64_windows_defaults_are_runner_owned_and_target_scoped() {
        let scratch = cortexkit_test_support::ScratchDir::new("arm64-windows-defaults");
        let root = scratch.path().join("llvm-mingw");
        std::fs::create_dir(&root).unwrap();
        let mut env = BTreeMap::from([
            ("CC".into(), "clang".into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            (
                "CC_x86_64_pc_windows_gnu".into(),
                "x86_64-w64-mingw32-gcc".into(),
            ),
        ]);
        let original = env.clone();
        for name in [
            "CC_aarch64_pc_windows_gnullvm",
            "AR_aarch64_pc_windows_gnullvm",
        ] {
            assert!(!crate::allowed_env(name), "caller must not override {name}");
        }
        let linker = "CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER";
        assert!(crate::allowed_env(linker));
        env.insert(linker.into(), "caller-linker".into());
        arm64_windows(&mut env, &root);
        for (name, binary) in [
            ("CC_aarch64_pc_windows_gnullvm", "aarch64-w64-mingw32-clang"),
            ("AR_aarch64_pc_windows_gnullvm", "llvm-ar"),
            (linker, "aarch64-w64-mingw32-clang"),
        ] {
            assert_eq!(
                env.remove(name).unwrap(),
                root.join("bin").join(binary).to_str().unwrap()
            );
        }
        assert_eq!(env, original);
    }

    #[test]
    fn old_images_without_llvm_mingw_get_no_arm64_windows_defaults() {
        let scratch = cortexkit_test_support::ScratchDir::new("arm64-windows-old-image");
        let mut env = BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]);
        let original = env.clone();
        arm64_windows(&mut env, &scratch.path().join("not-installed"));
        assert_eq!(env, original);
    }
}
