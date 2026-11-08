//! Compatibility inputs for format-1 build bases. Generated executables and
//! snapshot-selected toolchains are deliberately never run to identify a base.
use crate::{compile_cache, workspace};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io, path::Path, process::Command};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ToolchainIdentity {
    pub rustc_verbose: String,
    pub rust_toolchain: Option<String>,
    pub rust_toolchain_toml: Option<String>,
    pub wrapper: Option<WrapperIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum WrapperIdentity {
    Supervisor {
        source_sha256: String,
    },
    Sccache {
        script_sha256: String,
        backend_sha256: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct CompatibleInputs {
    pub toolchain: ToolchainIdentity,
    pub environment: String,
}

/// Unavailable is not a compatible identity and cannot be persisted as one.
#[derive(Debug)]
pub(crate) enum Compatibility {
    Available(CompatibleInputs),
    Unavailable(String),
}

impl Compatibility {
    /// Seeding must skip reuse on Unavailable and log its reason once for the
    /// job. Do not substitute an empty identity or compare two errors as equal.
    pub(crate) fn for_seed(&self) -> Result<&CompatibleInputs, &str> {
        self.available()
    }

    /// Publication must refuse Unavailable and log its reason once for the
    /// fill. Persist only the launch's Available inputs, including on restart.
    pub(crate) fn for_publication(&self) -> Result<&CompatibleInputs, &str> {
        self.available()
    }

    /// The due check must not count an unavailable identity as a difference;
    /// missing bases and changed tips can still independently make a fill due.
    pub(crate) fn differs_for_due(&self, published: &CompatibleInputs) -> bool {
        matches!(self, Self::Available(current) if current != published)
    }

    fn available(&self) -> Result<&CompatibleInputs, &str> {
        match self {
            Self::Available(inputs) => Ok(inputs),
            Self::Unavailable(reason) => Err(reason),
        }
    }
}

/// Probe once in the runner's own environment, never in a job's checkout. Keep
/// failure as data so an unreadable compiler disables reuse rather than jobs.
pub(crate) struct RunnerToolchain(Result<String, String>);

impl RunnerToolchain {
    pub(crate) fn probe() -> Self {
        Self(Self::read())
    }

    fn read() -> Result<String, String> {
        let output = Command::new("rustc")
            .arg("-vV")
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!("runner rustc -vV failed: {}", output.status));
        }
        let text = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
        if text.is_empty() {
            return Err("runner rustc -vV returned no identity".into());
        }
        Ok(text)
    }
}

/// All inputs exist before wrapper compilation. `env` must come from
/// backend::job_environment with the unit's selected wrapper and transport.
/// `sccache_backend` is the shared Cargo home's bin/sccache, not the generated
/// sandbox script or a command resolved in the job's PATH.
pub(crate) fn compatibility(
    runner: &RunnerToolchain,
    repository: &Path,
    tree: &str,
    env: &BTreeMap<String, String>,
    sccache_backend: &Path,
) -> Compatibility {
    let collect = || -> io::Result<CompatibleInputs> {
        let rustc_verbose = runner
            .0
            .as_ref()
            .map_err(|e| io::Error::other(e.clone()))?
            .clone();
        let entries = workspace::git(
            repository,
            &[
                "ls-tree",
                "--full-tree",
                "-z",
                tree,
                "--",
                "rust-toolchain",
                "rust-toolchain.toml",
            ],
        )?;
        let mut blobs = BTreeMap::new();
        for entry in entries.split('\0').filter(|entry| !entry.is_empty()) {
            let (metadata, name) = entry
                .split_once('\t')
                .ok_or_else(|| io::Error::other("invalid toolchain tree entry"))?;
            let fields: Vec<_> = metadata.split_whitespace().collect();
            if fields.len() != 3 || fields[1] != "blob" {
                return Err(io::Error::other(format!(
                    "toolchain input is not a blob: {name}"
                )));
            }
            blobs.insert(name, fields[2].to_owned());
        }
        let wrapper = match env.get("RUSTC_WRAPPER").map(String::as_str) {
            Some("/motor-tools/prefrontal-build-slot") => Some(WrapperIdentity::Supervisor {
                source_sha256: supervisor_digest(),
            }),
            Some("sccache" | "/motor-tools/sccache") => Some(WrapperIdentity::Sccache {
                script_sha256: format!(
                    "{:x}",
                    Sha256::digest(include_bytes!("sccache-wrapper.sh"))
                ),
                backend_sha256: format!("{:x}", Sha256::digest(fs::read(sccache_backend)?)),
            }),
            _ => None,
        };
        Ok(CompatibleInputs {
            toolchain: ToolchainIdentity {
                rustc_verbose,
                rust_toolchain: blobs.remove("rust-toolchain"),
                rust_toolchain_toml: blobs.remove("rust-toolchain.toml"),
                wrapper,
            },
            environment: compile_cache::environment_fingerprint(env),
        })
    };
    match collect() {
        Ok(inputs) => Compatibility::Available(inputs),
        Err(error) => Compatibility::Unavailable(error.to_string()),
    }
}

pub(crate) fn supervisor_digest() -> String {
    format!(
        "{:x}",
        Sha256::digest(include_bytes!("../../prefrontal-build-slot/src/main.rs"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    struct Fixture {
        scratch: ScratchDir,
        runner: RunnerToolchain,
        tree: String,
    }

    impl Fixture {
        fn new() -> Self {
            let scratch = ScratchDir::new("build-base-identity");
            workspace::git(&scratch, &["init", "-q"]).unwrap();
            fs::write(scratch.join("Cargo.toml"), "[workspace]\n").unwrap();
            workspace::git(&scratch, &["add", "Cargo.toml"]).unwrap();
            let tree = workspace::git(&scratch, &["write-tree"]).unwrap();
            Self {
                scratch,
                runner: RunnerToolchain(Ok(
                    "rustc test\nbinary: rustc\nhost: test-host\nrelease: test\n".into(),
                )),
                tree,
            }
        }

        fn identity(&self, wrapper: Option<&str>) -> Compatibility {
            let env = wrapper
                .map(|value| BTreeMap::from([("RUSTC_WRAPPER".into(), value.into())]))
                .unwrap_or_default();
            compatibility(
                &self.runner,
                &self.scratch,
                self.tree.trim(),
                &env,
                &self.scratch.join("sccache-backend"),
            )
        }

        fn stage(&mut self, path: &str, content: &str) {
            fs::write(self.scratch.join(path), content).unwrap();
            workspace::git(&self.scratch, &["add", path]).unwrap();
            self.tree = workspace::git(&self.scratch, &["write-tree"]).unwrap();
        }
    }

    #[test]
    fn identity_records_snapshot_root_blobs_absence_and_runner_verbose_version() {
        let mut f = Fixture::new();
        let original = f.identity(None).for_seed().unwrap().clone();
        assert_eq!(original.toolchain.rust_toolchain, None);
        assert_eq!(original.toolchain.rust_toolchain_toml, None);
        assert_eq!(
            original.toolchain.rustc_verbose,
            f.runner.0.as_ref().unwrap().as_str()
        );
        // Invalid toolchain text is only data: it must never be invoked.
        f.stage("rust-toolchain", "not-an-installed-toolchain\n");
        f.stage("rust-toolchain.toml", "also not a toolchain\n");
        let identity = f.identity(None).for_seed().unwrap().clone();
        for (name, actual) in [
            ("rust-toolchain", &identity.toolchain.rust_toolchain),
            (
                "rust-toolchain.toml",
                &identity.toolchain.rust_toolchain_toml,
            ),
        ] {
            let expected = workspace::git(
                &f.scratch,
                &["rev-parse", &format!("{}:{name}", f.tree.trim())],
            )
            .unwrap();
            assert_eq!(actual.as_deref(), Some(expected.trim()));
        }
        assert_ne!(identity, original);
        fs::write(f.scratch.join("rust-toolchain"), "unstaged edit").unwrap();
        assert_eq!(f.identity(None).for_seed().unwrap(), &identity);
        f.runner = RunnerToolchain(Ok("rustc same release\nhost: other-host\n".into()));
        assert!(f.identity(None).differs_for_due(&identity));
    }

    #[test]
    fn identity_is_stable_before_supervisor_build_and_after_rebuild() {
        let f = Fixture::new();
        let before = f
            .identity(Some("/motor-tools/prefrontal-build-slot"))
            .for_seed()
            .unwrap()
            .clone();
        assert_eq!(
            before.toolchain.wrapper,
            Some(WrapperIdentity::Supervisor {
                source_sha256: supervisor_digest()
            })
        );
        let source = f.scratch.join("supervisor.rs");
        let output = f.scratch.join("supervisor");
        fs::write(
            &source,
            include_str!("../../prefrontal-build-slot/src/main.rs"),
        )
        .unwrap();
        for _ in 0..2 {
            crate::platform::build_wrapper(&source, &output, &f.scratch.join("slots"), 2).unwrap();
            // A fill must record the same pre-setup source identity as a reader.
            assert_eq!(
                f.identity(Some("/motor-tools/prefrontal-build-slot"))
                    .for_publication()
                    .unwrap(),
                &before
            );
            fs::remove_file(&output).unwrap();
        }
    }

    #[test]
    fn sccache_identity_hashes_embedded_script_and_backend_not_generated_tools() {
        let f = Fixture::new();
        fs::write(f.scratch.join("sccache-backend"), b"backend one").unwrap();
        let before = f
            .identity(Some("/motor-tools/sccache"))
            .for_seed()
            .unwrap()
            .clone();
        assert_eq!(
            before.toolchain.wrapper,
            Some(WrapperIdentity::Sccache {
                script_sha256: format!(
                    "{:x}",
                    Sha256::digest(include_bytes!("sccache-wrapper.sh"))
                ),
                backend_sha256: format!("{:x}", Sha256::digest(b"backend one")),
            })
        );
        fs::write(f.scratch.join("sccache"), "generated wrapper copy").unwrap();
        assert_eq!(
            f.identity(Some("/motor-tools/sccache"))
                .for_publication()
                .unwrap(),
            &before
        );
        // The build-server spelling resolves to the same embedded wrapper.
        assert_eq!(
            f.identity(Some("sccache")).for_seed().unwrap().toolchain,
            before.toolchain
        );
        fs::write(f.scratch.join("sccache-backend"), b"backend two").unwrap();
        assert!(f
            .identity(Some("/motor-tools/sccache"))
            .differs_for_due(&before));
    }

    #[test]
    fn unreadable_inputs_are_unavailable_not_compatible_or_due_differences() {
        let mut f = Fixture::new();
        let published = f.identity(None).for_seed().unwrap().clone();
        let assert_unavailable = |result: Compatibility| {
            assert!(matches!(result, Compatibility::Unavailable(_)));
            assert!(!result.for_seed().unwrap_err().is_empty());
            assert!(!result.for_publication().unwrap_err().is_empty());
            assert!(!result.differs_for_due(&published));
        };
        assert_unavailable(f.identity(Some("/motor-tools/sccache")));
        fs::create_dir(f.scratch.join("sccache-backend")).unwrap();
        assert_unavailable(f.identity(Some("sccache")));
        f.tree = "missing-tree".into();
        assert_unavailable(f.identity(None));
        f.tree = workspace::git(&f.scratch, &["write-tree"]).unwrap();
        f.runner = RunnerToolchain(Err("runner rustc unreadable".into()));
        assert_unavailable(f.identity(None));
    }

    #[test]
    fn runner_probe_uses_verbose_host_compiler_output() {
        let runner = RunnerToolchain::probe();
        let text = runner.0.unwrap();
        assert!(text.contains("\nbinary: rustc\n"));
        assert!(text.contains("\nhost: "));
        assert!(text.contains("\nrelease: "));
    }
}
