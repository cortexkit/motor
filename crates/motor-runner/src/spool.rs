use crate::storage::invalid;
use motor_protocol::{BundleManifest, Frame};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Write},
    path::PathBuf,
};

/// An upload owns its spools until verification of the entire manifest. There
/// is deliberately no fetch callback here: an earlier verified bundle must not
/// leak objects into a mirror while a later bundle is still in transit.
pub(crate) struct Upload {
    pub directory: PathBuf,
    manifest: Vec<BundleManifest>,
    current: usize,
    offset: u64,
    digest: Sha256,
    file: Option<File>,
}

impl Upload {
    pub fn new(directory: PathBuf, manifest: &[BundleManifest]) -> io::Result<Self> {
        for (i, b) in manifest.iter().enumerate() {
            if b.index as usize != i
                || b.sha256.len() != 64
                || !b.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            {
                return Err(invalid("invalid bundle manifest"));
            }
        }
        fs::create_dir_all(&directory)?;
        let mut this = Self {
            directory,
            manifest: manifest.to_vec(),
            current: 0,
            offset: 0,
            digest: Sha256::new(),
            file: None,
        };
        this.advance_empty()?;
        Ok(this)
    }
    pub fn complete(&self) -> bool {
        self.current == self.manifest.len()
    }
    fn advance_empty(&mut self) -> io::Result<()> {
        while !self.complete() {
            self.file = Some(File::create(self.directory.join(self.current.to_string()))?);
            if self.manifest[self.current].length != 0 {
                break;
            }
            self.finish_bundle()?;
        }
        Ok(())
    }
    fn finish_bundle(&mut self) -> io::Result<()> {
        if format!("{:x}", self.digest.clone().finalize()) != self.manifest[self.current].sha256 {
            return Err(invalid("bundle digest mismatch"));
        }
        self.file.take().unwrap().sync_all()?;
        self.current += 1;
        self.offset = 0;
        self.digest = Sha256::new();
        Ok(())
    }
    pub fn chunk(&mut self, frame: Frame) -> io::Result<()> {
        let Frame::BundleChunk {
            index,
            offset,
            data,
        } = frame
        else {
            return Err(invalid("expected bundle chunk"));
        };
        if self.complete() || index as usize != self.current || offset != self.offset {
            return Err(invalid("noncontiguous bundle chunk"));
        }
        let length = self.manifest[self.current].length;
        if data.0.len() as u64 > length - self.offset {
            return Err(invalid("bundle overrun"));
        }
        self.file.as_mut().unwrap().write_all(&data.0)?;
        self.digest.update(&data.0);
        self.offset += data.0.len() as u64;
        if self.offset == length {
            self.finish_bundle()?;
            self.advance_empty()?;
        }
        Ok(())
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
