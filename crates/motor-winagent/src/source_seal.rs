//! Bounded lists of snapshot files to protect across transferred repositories.
//! Windows enforces the read-only policy using file and parent-directory DACLs.

use std::collections::BTreeSet;
use std::io;

use sha2::{Digest, Sha256};

#[cfg(windows)]
mod native;
#[cfg(windows)]
pub use native::{apply, refresh};

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub const MAX_SOURCE_FILES: usize = 100_000;
pub const MAX_SOURCE_PATH_BYTES: usize = 32 * 1024 * 1024;
pub const PLAN_LIMIT_REASON: &str = "source_seal_plan_limit";

fn check_budget(count: usize, bytes: usize) -> io::Result<()> {
    if count > MAX_SOURCE_FILES || bytes > MAX_SOURCE_PATH_BYTES {
        return Err(invalid(&format!(
            "{PLAN_LIMIT_REASON}: at most {MAX_SOURCE_FILES} files and {MAX_SOURCE_PATH_BYTES} UTF-8 path bytes"
        )));
    }
    Ok(())
}

/// Bounds the complete conversation, not just each independently bounded frame.
/// Used separately by host selection, protocol validation, and guest receipt.
#[derive(Debug, Default)]
pub struct PlanBuilder {
    files: Vec<String>,
    path_bytes: usize,
}

impl PlanBuilder {
    pub fn push(&mut self, file: String) -> io::Result<()> {
        self.extend(vec![file])
    }

    pub fn extend(&mut self, files: Vec<String>) -> io::Result<()> {
        let count = self.files.len().saturating_add(files.len());
        let bytes = files.iter().fold(self.path_bytes, |total, file| {
            total.saturating_add(file.len())
        });
        check_budget(count, bytes)?;
        self.files.extend(files);
        self.path_bytes = bytes;
        Ok(())
    }

    pub fn finish(self) -> io::Result<Plan> {
        Plan::new(self.files)
    }
}

/// Paths are relative to C:\m, not the command's cwd or the cache volume.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub files: Vec<String>,
}

impl Plan {
    pub fn new(mut files: Vec<String>) -> io::Result<Self> {
        check_budget(
            files.len(),
            files
                .iter()
                .fold(0usize, |total, file| total.saturating_add(file.len())),
        )?;
        let mut names = BTreeSet::new();
        let mut prefixes = std::collections::BTreeMap::new();
        for file in &files {
            crate::returned::validate_relative(file)
                .map_err(|_| invalid("invalid source seal path"))?;
            if file
                .split('/')
                .any(|name| crate::bun::is_node_modules(name.as_ref()))
            {
                return Err(invalid("dependency path in source seal plan"));
            }
            if !names.insert(file.to_lowercase()) {
                return Err(invalid("case collision in source seal plan"));
            }
            let mut prefix = String::new();
            for component in file.split('/') {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                if let Some(original) = prefixes.insert(prefix.to_lowercase(), prefix.clone())
                    && original != prefix
                {
                    return Err(invalid("case collision in source seal ancestors"));
                }
            }
        }
        files.sort();
        Ok(Self { files })
    }

    pub fn identity(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(b"motor-windows-source-seal-v1\0");
        for file in &self.files {
            hash.update((file.len() as u64).to_be_bytes());
            hash.update(file.as_bytes());
        }
        format!("{:x}", hash.finalize())
    }

    pub fn count(&self) -> u64 {
        self.files.len() as u64
    }

    pub fn matches(&self, identity: &str, count: u64) -> bool {
        count == self.count() && identity == self.identity()
    }

    /// Only trusted selected ancestors are visited; generated trees are not walked.
    pub fn directories(&self) -> BTreeSet<String> {
        let mut directories = BTreeSet::new();
        for file in &self.files {
            directories.insert(String::new());
            let mut rest = file.as_str();
            while let Some((parent, _)) = rest.rsplit_once('/') {
                directories.insert(parent.to_owned());
                rest = parent;
            }
        }
        directories
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, Frame, PROTOCOL_VERSION, Peer, Protocol, State};

    #[test]
    fn plan_identity_and_ancestors() {
        let plan = Plan::new(vec!["repo/src/a.rs".into(), "repo/b".into()]).unwrap();
        assert_eq!(
            plan.directories(),
            ["", "repo", "repo/src"].map(String::from).into()
        );
        assert!(plan.matches(&plan.identity(), 2));
        assert!(!plan.matches(&plan.identity(), 1));
        assert!(!plan.matches(&"0".repeat(64), 2));
        assert_ne!(plan.identity(), Plan::default().identity());
    }

    #[test]
    fn rejects_windows_aliases_and_dependencies() {
        for files in [
            vec!["repo/A", "repo/a"],
            vec!["repo/A/one", "repo/a/two"],
            vec!["repo/a:stream"],
            vec!["repo/a."],
            vec!["repo/a "],
            vec!["../a"],
            vec!["repo/node_modules/a"],
        ] {
            assert!(Plan::new(files.into_iter().map(String::from).collect()).is_err());
        }
    }

    fn ready() -> Protocol {
        let mut protocol = Protocol::new("nonce").unwrap();
        for (peer, frame) in [
            (
                Peer::Agent,
                Frame::Hello {
                    version: PROTOCOL_VERSION,
                    nonce: "nonce".into(),
                },
            ),
            (
                Peer::Host,
                Frame::SyncClock {
                    unix_ms: 1_800_000_000_000,
                },
            ),
            (Peer::Agent, Frame::ClockSynchronized { correction_ms: 0 }),
            (
                Peer::Host,
                Frame::TreeEnd {
                    command: Command {
                        command: "true".into(),
                        cwd: "/repo".into(),
                        workspace: "/repo".into(),
                        source_commit: "commit".into(),
                        env: Default::default(),
                    },
                },
            ),
            (Peer::Agent, Frame::TreeReceived {}),
        ] {
            protocol.accept(peer, &frame).unwrap();
        }
        protocol
    }

    #[test]
    fn start_requires_complete_matching_acknowledgement() {
        assert!(ready().accept(Peer::Host, &Frame::Start {}).is_err());
        let plan = Plan::new(vec!["repo/source".into()]).unwrap();
        for mismatch in 0..4 {
            let mut protocol = ready();
            protocol
                .accept(
                    Peer::Host,
                    &Frame::SealSource {
                        files: plan.files.clone(),
                    },
                )
                .unwrap();
            protocol
                .accept(
                    Peer::Host,
                    &Frame::SealSourceEnd {
                        identity: plan.identity(),
                        count: 1,
                    },
                )
                .unwrap();
            match mismatch {
                0 => assert!(protocol.accept(Peer::Host, &Frame::Start {}).is_err()),
                1 => assert!(
                    protocol
                        .accept(
                            Peer::Agent,
                            &Frame::SourceSealed {
                                identity: "0".repeat(64),
                                count: 1
                            }
                        )
                        .is_err()
                ),
                2 => assert!(
                    protocol
                        .accept(
                            Peer::Agent,
                            &Frame::SourceSealed {
                                identity: plan.identity(),
                                count: 0
                            }
                        )
                        .is_err()
                ),
                _ => {
                    protocol
                        .accept(
                            Peer::Agent,
                            &Frame::SourceSealed {
                                identity: plan.identity(),
                                count: 1,
                            },
                        )
                        .unwrap();
                    protocol.accept(Peer::Host, &Frame::Start {}).unwrap();
                    assert_eq!(protocol.state(), State::AwaitStarted);
                }
            }
        }
        let mut protocol = ready();
        protocol
            .accept(
                Peer::Host,
                &Frame::SealSource {
                    files: plan.files.clone(),
                },
            )
            .unwrap();
        protocol
            .accept(
                Peer::Host,
                &Frame::SealSource {
                    files: vec!["repo/SOURCE".into()],
                },
            )
            .unwrap();
        assert!(
            protocol
                .accept(
                    Peer::Host,
                    &Frame::SealSourceEnd {
                        identity: plan.identity(),
                        count: 1
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn seal_chunks_have_aggregate_limits_before_start() {
        for byte_limit in [false, true] {
            let chunk: Vec<String> = if byte_limit {
                vec![format!("repo/{}", "x".repeat(64 * 1024 - 5))]
            } else {
                (0..10_000).map(|i| format!("repo/file-{i}")).collect()
            };
            let frame = Frame::SealSource {
                files: chunk.clone(),
            };
            // Each individual frame is legal; only the accumulated plan reaches
            // the boundary. Both protocol and guest collection use the builder.
            crate::write_frame(&mut Vec::new(), &frame).unwrap();
            let batches = if byte_limit {
                MAX_SOURCE_PATH_BYTES / chunk[0].len()
            } else {
                MAX_SOURCE_FILES / chunk.len()
            };
            let mut protocol = ready();
            let mut guest = PlanBuilder::default();
            for _ in 0..batches {
                protocol.accept(Peer::Host, &frame).unwrap();
                guest.extend(chunk.clone()).unwrap();
            }
            let before = (guest.files.len(), guest.path_bytes);
            let guest_error = guest.extend(chunk.clone()).unwrap_err();
            assert!(guest_error.to_string().contains(PLAN_LIMIT_REASON));
            assert_eq!(
                (guest.files.len(), guest.path_bytes),
                before,
                "rejected chunks must not be partially retained"
            );
            let error = protocol.accept(Peer::Host, &frame).unwrap_err();
            assert!(error.to_string().contains(PLAN_LIMIT_REASON));
            assert_eq!(protocol.state(), State::Failed);
            assert!(protocol.accept(Peer::Host, &Frame::Start {}).is_err());
        }
    }

    #[test]
    fn direct_plans_reject_count_and_byte_overflow_with_named_reason() {
        for files in [
            vec!["repo/file".to_owned(); MAX_SOURCE_FILES + 1],
            vec!["x".repeat(MAX_SOURCE_PATH_BYTES + 1)],
        ] {
            let error = Plan::new(files).unwrap_err();
            assert!(error.to_string().contains(PLAN_LIMIT_REASON));
        }
    }
}
