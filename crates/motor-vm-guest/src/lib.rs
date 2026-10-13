//! Guest supervisor protocol. Frames are big-endian u32 length + strict JSON.
pub mod fetch;
#[cfg(target_os = "linux")]
pub mod linux;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

pub const MAX_FRAME: usize = 1024 * 1024;
pub const OUTPUT_CHUNK: usize = 8192;
pub const TARGET: &str = "/run/motor-vm/target";
pub const CARGO_HOME: &str = "/run/motor-vm/cargo";
/// All caller-selected mounts live below this root, away from image binaries and configuration.
pub const JOB_ROOT: &str = "/job";
pub fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeMount {
    pub subdirectory: String,
    pub destination: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Mode {
    Job {
        command: Vec<String>,
        cwd: PathBuf,
    },
    /// Cargo and git are pinned image binaries, never rustup proxies or tree binaries.
    Fetch {
        manifest: PathBuf,
        git_directories: BTreeMap<String, String>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub realtime_seconds: i64,
    pub realtime_nanoseconds: u32,
    pub entropy: [u8; 32],
    pub trees: Vec<TreeMount>,
    pub env: BTreeMap<String, String>,
    pub vcpus: u32,
    // MicroVM backend slice s5 (the helper relay) must choose a port >= 1024.
    pub proxy_port: Option<u32>,
    pub mode: Mode,
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Termination {
    Exit { code: i32 },
    Signal { signal: i32 },
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Stdout {
        bytes: Vec<u8>,
    },
    Stderr {
        bytes: Vec<u8>,
    },
    Status {
        termination: Termination,
        clean: bool,
    },
}
pub fn read_frame<T: serde::de::DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(invalid("frame exceeds bound"));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(|e| invalid(&e.to_string()))
}
pub fn write_frame(writer: &mut impl Write, frame: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(frame).map_err(|e| invalid(&e.to_string()))?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(invalid("frame exceeds bound"));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}
pub fn absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}
impl Start {
    pub fn validate(&self) -> io::Result<()> {
        if self.realtime_seconds < 0
            || self.realtime_nanoseconds >= 1_000_000_000
            || !(1..=256).contains(&self.vcpus)
            || self.trees.is_empty()
            || self.trees.len() > 64
            || self.proxy_port.is_some_and(|p| p < 1024)
        {
            return Err(invalid("invalid start settings"));
        }
        for (i, tree) in self.trees.iter().enumerate() {
            if tree.subdirectory.is_empty()
                || !tree
                    .subdirectory
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
                || !absolute(&tree.destination)
                || !tree.destination.starts_with(JOB_ROOT)
                || tree.destination == Path::new(JOB_ROOT)
                || self.trees[..i].iter().any(|t| {
                    tree.destination.starts_with(&t.destination)
                        || t.destination.starts_with(&tree.destination)
                })
            {
                return Err(invalid("unsafe or overlapping tree mount"));
            }
        }
        let inside_tree =
            |p: &Path| absolute(p) && self.trees.iter().any(|t| p.starts_with(&t.destination));
        match &self.mode {
            Mode::Job { command, cwd }
                if command.is_empty()
                    || command.iter().any(|a| a.contains('\0'))
                    || !inside_tree(cwd) =>
            {
                return Err(invalid("invalid job command or cwd"))
            }
            Mode::Fetch { manifest, .. } if !inside_tree(manifest) || self.proxy_port.is_none() => {
                return Err(invalid("fetch requires a manifest and relay"))
            }
            _ => {}
        }
        if self
            .env
            .iter()
            .any(|(k, v)| k.is_empty() || k.contains(['=', '\0']) || v.contains('\0'))
        {
            return Err(invalid("invalid environment"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
