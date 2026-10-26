use std::{
    io,
    path::{Component, Path, PathBuf},
};

#[derive(Clone)]
pub(crate) struct Checks {
    exists: fn(&Path) -> bool,
    env_present: fn(&str) -> bool,
    read: fn(&Path) -> io::Result<String>,
    canonicalize: fn(&Path) -> io::Result<PathBuf>,
}

impl Default for Checks {
    fn default() -> Self {
        Self {
            exists: Path::exists,
            env_present: |name| std::env::var_os(name).is_some(),
            read: |path| std::fs::read_to_string(path),
            canonicalize: |path| std::fs::canonicalize(path),
        }
    }
}

impl Checks {
    pub(crate) fn refusal<'a>(&self, paths: impl IntoIterator<Item = &'a Path>) -> Option<String> {
        let active = (self.exists)(Path::new("/proc/sys/fs/binfmt_misc/WSLInterop"))
            || (self.exists)(Path::new("/proc/sys/fs/binfmt_misc/WSLInterop-late"))
            || (self.env_present)("WSL_DISTRO_NAME")
            || (self.read)(Path::new("/proc/sys/kernel/osrelease"))
                .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"));
        if !active {
            return None;
        }
        paths.into_iter().find_map(|path| {
            let drive = windows_drive(path)
                || (self.canonicalize)(path).is_ok_and(|resolved| windows_drive(&resolved));
            drive.then(|| format!(
                "ck-motor: this checkout is on a Windows drive ({}). Under WSL, move it into the WSL filesystem (for example under ~/) and run ck motor there.",
                path.display()
            ))
        })
    }
}

fn windows_drive(path: &Path) -> bool {
    // Resolve lexical parent components before inspecting the mount name.
    // Checking the supplied path separately from its canonical target rejects
    // /mnt/<letter>/ paths even when a symlink resolves outside the drive.
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    let mut components = normalized.components();
    matches!(components.next(), Some(Component::RootDir))
        && components.next() == Some(Component::Normal("mnt".as_ref()))
        && components.next().is_some_and(|drive| {
            let name = drive.as_os_str().as_encoded_bytes();
            name.len() == 1 && name[0].is_ascii_alphabetic()
        })
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Probe {
    Interop,
    InteropLate,
    DistroName,
    Release,
    None,
}

#[cfg(test)]
impl Checks {
    pub(crate) fn test(probe: Probe) -> Self {
        Self {
            exists: match probe {
                Probe::Interop => |path| path == Path::new("/proc/sys/fs/binfmt_misc/WSLInterop"),
                Probe::InteropLate => {
                    |path| path == Path::new("/proc/sys/fs/binfmt_misc/WSLInterop-late")
                }
                _ => |_| false,
            },
            env_present: match probe {
                Probe::DistroName => |name| name == "WSL_DISTRO_NAME",
                _ => |_| false,
            },
            read: match probe {
                Probe::Release => |path| {
                    assert_eq!(path, Path::new("/proc/sys/kernel/osrelease"));
                    Ok("6.6.1-MiCrOsOfT-standard-WSL2".into())
                },
                _ => |_| Ok("6.6.1-generic".into()),
            },
            ..Self::default()
        }
    }

    pub(crate) fn with_canonicalize(
        mut self,
        canonicalize: fn(&Path) -> io::Result<PathBuf>,
    ) -> Self {
        self.canonicalize = canonicalize;
        self
    }
}

#[cfg(test)]
mod tests;
