use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Where the job image installs llvm-mingw; absent on images built before it.
#[cfg(not(test))]
const ROOT: &str = "/usr/local/lib/motor/llvm-mingw";

#[cfg(test)]
thread_local! {
    /// A test's own llvm-mingw root. Unset, test builds see none, so pinned
    /// argv and environment tests don't depend on the job image running them.
    pub(crate) static TEST_ROOT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// The llvm-mingw root the job environment and the launch both use.
pub(crate) fn root() -> PathBuf {
    #[cfg(test)]
    return TEST_ROOT
        .with(|root| root.borrow().clone())
        .unwrap_or_else(|| PathBuf::from("/nonexistent/motor-test-llvm-mingw"));
    #[cfg(not(test))]
    PathBuf::from(ROOT)
}

pub(crate) fn arm64_windows(env: &mut BTreeMap<String, String>, root: &Path) {
    if !root.is_dir() {
        return;
    }
    // cc-rs needs the image's Universal C Runtime headers for ARM64 Windows
    // GNU/LLVM, not host clang or a Microsoft SDK. Callers can't send CC_/AR_
    // variables, so the runner supplies them; a linker the caller set
    // explicitly is kept. Target-scoped names leave every other compiler
    // selection alone.
    for (name, binary) in [
        ("CC_aarch64_pc_windows_gnullvm", "aarch64-w64-mingw32-clang"),
        ("AR_aarch64_pc_windows_gnullvm", "llvm-ar"),
        (
            "CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER",
            "aarch64-w64-mingw32-clang",
        ),
    ] {
        env.entry(name.into())
            .or_insert_with(|| root.join("bin").join(binary).to_string_lossy().into_owned());
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

        // A linker the caller chose explicitly is kept.
        env.insert(linker.into(), "caller-linker".into());
        arm64_windows(&mut env, &root);
        assert_eq!(env[linker], "caller-linker");
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
