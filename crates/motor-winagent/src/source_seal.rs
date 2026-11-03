//! The authenticated, primary-workspace source plan and its Windows enforcement.

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

/// Paths are relative to C:\m, not the command's cwd or the cache volume.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub files: Vec<String>,
}

impl Plan {
    pub fn new(mut files: Vec<String>) -> io::Result<Self> {
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

    #[test]
    fn start_requires_complete_matching_acknowledgement() {
        use crate::{Command, Frame, PROTOCOL_VERSION, Peer, Protocol, State};
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
}
