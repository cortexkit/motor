use std::collections::BTreeSet;
use std::path::PathBuf;

/// Observes Cargo JSON without requesting it or modifying the job's output.
/// `compiler-artifact` carries Cargo's own `fresh` decision and manifest path;
/// `build-finished` closes the first build, including builds with no compilations.
/// Human output, incomplete streams and oversized records give no known count.
pub(super) struct FirstBuild {
    workspace: PathBuf,
    pending: Vec<u8>,
    compiled: BTreeSet<PathBuf>,
    saw_local: bool,
    done: bool,
    count: Option<usize>,
}

impl FirstBuild {
    pub(super) fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            pending: Vec::new(),
            compiled: BTreeSet::new(),
            saw_local: false,
            done: false,
            count: None,
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) -> Option<usize> {
        if self.done {
            return None;
        }
        for &byte in bytes {
            if byte != b'\n' {
                if self.pending.len() == 64 * 1024 {
                    self.done = true;
                    self.pending.clear();
                    return None;
                }
                self.pending.push(byte);
                continue;
            }
            let value = serde_json::from_slice::<serde_json::Value>(&self.pending);
            self.pending.clear();
            let Ok(value) = value else { continue };
            match value["reason"].as_str() {
                Some("compiler-artifact") => {
                    let (Some(manifest), Some(fresh)) =
                        (value["manifest_path"].as_str(), value["fresh"].as_bool())
                    else {
                        self.done = true;
                        return None;
                    };
                    let manifest = PathBuf::from(manifest);
                    if manifest.starts_with(&self.workspace) {
                        self.saw_local = true;
                        if !fresh {
                            // A package's build script and library are one local crate.
                            self.compiled.insert(manifest);
                            if self.compiled.len() > 4096 {
                                self.done = true;
                                self.compiled.clear();
                                return None;
                            }
                        }
                    }
                }
                Some("build-finished") => {
                    self.done = true;
                    self.count = (self.saw_local && value["success"].as_bool() == Some(true))
                        .then_some(self.compiled.len());
                    return self.count;
                }
                _ => {}
            }
        }
        None
    }

    #[cfg(test)]
    pub(super) fn count(&self) -> Option<usize> {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_local_packages_once_at_first_successful_build_boundary() {
        let mut build = FirstBuild::new("/caller/repo".into());
        let stream = concat!(
            "{\"reason\":\"compiler-artifact\",\"manifest_path\":\"/caller/repo/a/Cargo.toml\",\"fresh\":false}\n",
            "{\"reason\":\"compiler-artifact\",\"manifest_path\":\"/caller/repo/a/Cargo.toml\",\"fresh\":false}\n",
            "{\"reason\":\"compiler-artifact\",\"manifest_path\":\"/caller/repo/b/Cargo.toml\",\"fresh\":true}\n",
            "{\"reason\":\"compiler-artifact\",\"manifest_path\":\"/caller/repository/Cargo.toml\",\"fresh\":false}\n",
            "{\"reason\":\"build-finished\",\"success\":true}\n"
        );
        let counts: Vec<_> = stream
            .as_bytes()
            .chunks(7)
            .filter_map(|b| build.push(b))
            .collect();
        assert_eq!(counts, [1]);
        assert_eq!(build.push(stream.as_bytes()), None);
    }

    #[test]
    fn unknown_failed_and_oversized_output_never_reports_zero() {
        for stream in [
            "    Finished `dev` profile\n".to_string(),
            "{\"reason\":\"build-finished\",\"success\":true}\n".to_string(),
            "x".repeat(64 * 1024 + 1),
            concat!(
                "{\"reason\":\"compiler-artifact\",\"manifest_path\":\"/repo/Cargo.toml\",\"fresh\":true}\n",
                "{\"reason\":\"build-finished\",\"success\":false}\n"
            ).to_string(),
        ] {
            assert_eq!(FirstBuild::new("/repo".into()).push(stream.as_bytes()), None);
        }
    }
}
