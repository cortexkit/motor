//! Only trusted Cargo runs during fetch; validate everything before publishing its image.
use crate::invalid;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize)]
struct Lock {
    package: Vec<Package>,
}
#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
}
// Exact bytes captured from cargo 1.99.0 by trusted_cargo_fetch_generation_satisfies_allowlist.
pub const CARGO_CACHE_TAG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by cargo.\n# For information about cache directory tags see https://bford.info/cachedir/\n";

pub struct Policy {
    registry: BTreeMap<String, String>,
    git: BTreeMap<String, BTreeSet<String>>,
    config: Vec<u8>,
}
fn plain(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0'])
}
impl Policy {
    /// git_directories maps exact locked git sources (including #revision) to Cargo's
    /// directory name. These inputs come from the client's locked git bundles,
    /// prepared as bare repositories for this guest, not a runner git_sources table.
    pub fn from_lock(
        lock: &str,
        git_directories: &BTreeMap<String, String>,
        config: Vec<u8>,
    ) -> io::Result<Self> {
        let lock: Lock = toml::from_str(lock).map_err(|e| invalid(&e.to_string()))?;
        let mut registry = BTreeMap::new();
        let mut git: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut sources = BTreeSet::new();
        for p in lock.package {
            if !plain(&p.name) || !plain(&p.version) {
                return Err(invalid("invalid lock package name"));
            }
            match p.source {
                Some(source)
                    if source.starts_with("registry+") || source.starts_with("sparse+") =>
                {
                    let checksum = p
                        .checksum
                        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                        .ok_or_else(|| invalid("registry entry missing checksum"))?;
                    let name = format!("{}-{}", p.name, p.version);
                    if registry
                        .insert(name, checksum.clone())
                        .is_some_and(|old| old != checksum)
                    {
                        return Err(invalid("ambiguous registry checksum"));
                    }
                }
                Some(source) if source.starts_with("git+") => {
                    let rev = source
                        .rsplit_once('#')
                        .map(|(_, r)| r)
                        .filter(|r| r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit()))
                        .ok_or_else(|| invalid("git source missing locked revision"))?;
                    let directory = git_directories
                        .get(&source)
                        .filter(|d| plain(d))
                        .ok_or_else(|| invalid("unmapped git source"))?;
                    git.entry(directory.clone())
                        .or_default()
                        .insert(rev.to_owned());
                    sources.insert(source);
                }
                Some(_) => return Err(invalid("unsupported lock source")),
                None => {}
            }
        }
        if git_directories.keys().any(|s| !sources.contains(s)) {
            return Err(invalid("extra git mapping"));
        }
        Ok(Self {
            registry,
            git,
            config,
        })
    }

    pub fn validate(&self, home: &Path, git_binary: &Path) -> io::Result<()> {
        // No symlinks, devices or sockets can escape the generation's allowlist.
        check_tree(home)?;
        for entry in fs::read_dir(home)? {
            let entry = entry?;
            let name = name(&entry.path())?;
            match name.as_str() {
                "config.toml"
                    if entry.file_type()?.is_file() && fs::read(entry.path())? == self.config => {}
                ".global-cache" if entry.file_type()?.is_file() => {}
                n if n.starts_with(".package-cache") && entry.file_type()?.is_file() => {}
                "registry" if entry.file_type()?.is_dir() => self.registry(&entry.path())?,
                "git" if entry.file_type()?.is_dir() => self.git(&entry.path(), git_binary)?,
                _ => return Err(invalid("unexpected CARGO_HOME entry")),
            }
        }
        if fs::read(home.join("config.toml"))? != self.config {
            return Err(invalid("cargo config changed"));
        }
        Ok(())
    }
    fn registry(&self, root: &Path) -> io::Result<()> {
        for kind in fs::read_dir(root)? {
            let kind = kind?;
            let k = name(&kind.path())?;
            if k == "CACHEDIR.TAG"
                && kind.file_type()?.is_file()
                && fs::read(kind.path())? == CARGO_CACHE_TAG
            {
                continue;
            }
            if !kind.file_type()?.is_dir() || !["index", "cache", "src"].contains(&k.as_str()) {
                return Err(invalid("unexpected registry entry"));
            }
            if k == "index" {
                continue;
            }
            for index in fs::read_dir(kind.path())? {
                let index = index?;
                if !index.file_type()?.is_dir() {
                    return Err(invalid("invalid registry index directory"));
                }
                for package in fs::read_dir(index.path())? {
                    let package = package?;
                    let n = name(&package.path())?;
                    let key = if k == "cache" {
                        n.strip_suffix(".crate")
                            .ok_or_else(|| invalid("non-crate cache file"))?
                    } else {
                        &n
                    };
                    let checksum = self
                        .registry
                        .get(key)
                        .ok_or_else(|| invalid("crate absent from lock"))?;
                    let archive = if k == "cache" {
                        if !package.file_type()?.is_file() {
                            return Err(invalid("non-file crate archive"));
                        }
                        package.path()
                    } else {
                        if !package.file_type()?.is_dir() {
                            return Err(invalid("non-directory extracted source"));
                        }
                        root.join("cache")
                            .join(index.file_name())
                            .join(format!("{key}.crate"))
                    };
                    if checksum_file(&archive)? != checksum.to_ascii_lowercase() {
                        return Err(invalid("crate checksum mismatch"));
                    }
                }
            }
        }
        Ok(())
    }
    fn git(&self, root: &Path, binary: &Path) -> io::Result<()> {
        for kind in fs::read_dir(root)? {
            let kind = kind?;
            let k = name(&kind.path())?;
            if k == "CACHEDIR.TAG"
                && kind.file_type()?.is_file()
                && fs::read(kind.path())? == CARGO_CACHE_TAG
            {
                continue;
            }
            if !kind.file_type()?.is_dir() || !["db", "checkouts"].contains(&k.as_str()) {
                return Err(invalid("unexpected git entry"));
            }
            for directory in fs::read_dir(kind.path())? {
                let directory = directory?;
                let revisions = self
                    .git
                    .get(&name(&directory.path())?)
                    .ok_or_else(|| invalid("unlocked git directory"))?;
                if !directory.file_type()?.is_dir() {
                    return Err(invalid("invalid git directory"));
                }
                if k == "db" {
                    continue;
                }
                for checkout in fs::read_dir(directory.path())? {
                    let checkout = checkout?;
                    let short = name(&checkout.path())?;
                    if !checkout.file_type()?.is_dir()
                        || short.len() < 7
                        || !revisions.iter().any(|r| r.starts_with(&short))
                    {
                        return Err(invalid("unlocked git checkout"));
                    }
                    let result = Command::new(binary)
                        .env_clear()
                        .env("PATH", "/usr/bin:/bin")
                        .args([
                            "--no-optional-locks",
                            "-c",
                            "core.fsmonitor=false",
                            "-c",
                            "core.hooksPath=/dev/null",
                            "-c",
                            "safe.directory=*",
                        ])
                        .arg("-C")
                        .arg(checkout.path())
                        .args(["rev-parse", "--verify", "HEAD"])
                        .output()?;
                    let head = String::from_utf8(result.stdout)
                        .map_err(|_| invalid("invalid git revision"))?;
                    if !result.status.success()
                        || !revisions.contains(head.trim())
                        || !head.trim().starts_with(&short)
                    {
                        return Err(invalid("checkout revision mismatch"));
                    }
                }
            }
        }
        Ok(())
    }
}
fn name(path: &Path) -> io::Result<String> {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .ok_or_else(|| invalid("non-UTF8 cargo path"))
}
fn check_tree(root: &Path) -> io::Result<()> {
    let mut queue: Vec<PathBuf> = vec![root.to_owned()];
    let mut count = 0usize;
    while let Some(path) = queue.pop() {
        count += 1;
        if count > 1_000_000 {
            return Err(invalid("cargo path bound exceeded"));
        }
        let ty = fs::symlink_metadata(&path)?.file_type();
        if path.file_name().is_some_and(|n| n == "CACHEDIR.TAG")
            && path != root.join("registry/CACHEDIR.TAG")
            && path != root.join("git/CACHEDIR.TAG")
        {
            return Err(invalid("unexpected cache tag location"));
        }
        if ty.is_dir() {
            for entry in fs::read_dir(path)? {
                queue.push(entry?.path());
            }
        } else if !ty.is_file() {
            return Err(invalid("non-regular cargo path"));
        }
    }
    Ok(())
}
pub fn checksum_file(path: &Path) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut file = fs::File::open(path)?;
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
