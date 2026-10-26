//! The boundary between wire/catalog spellings and host filesystem paths.
//!
//! Wire and catalog keys stay in caller form. Authorization resolves mapped
//! destinations on the host without consulting the caller's filesystem tree.
use crate::storage::{BaseRequest, Job, LegacyCheckout, RetainedTransfer, Workspace};
use motor_protocol::{Prepare, Run, SiblingSnapshot};
use std::path::Path;
use std::{fs, io, path::PathBuf};
pub(crate) mod catalog;
pub(crate) mod owner;
pub(crate) mod startup;
pub use startup::CallerRoot;
#[cfg(all(test, target_os = "linux"))]
mod authorization_tests;
#[cfg(test)]
mod backstop_tests;
#[cfg(all(test, target_os = "linux"))]
mod startup_tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CallerPath<'a>(&'a String);

impl<'a> CallerPath<'a> {
    pub(crate) fn text(self) -> &'a String {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HostPath<'a>(&'a Path);

impl<'a> HostPath<'a> {
    pub(crate) fn as_path(self) -> &'a Path {
        self.0
    }
}

pub(crate) fn host(path: &Path) -> HostPath<'_> {
    HostPath(path)
}

pub(crate) fn host_str(value: &str) -> HostPath<'_> {
    HostPath(Path::new(value))
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PathPlan<'a> {
    pub(crate) caller: CallerPath<'a>,
    pub(crate) host: HostPath<'a>,
}

pub(crate) fn identity(value: &String) -> PathPlan<'_> {
    PathPlan {
        caller: CallerPath(value),
        host: HostPath(Path::new(value)),
    }
}

pub(crate) fn entries<T>(
    records: &std::collections::BTreeMap<String, T>,
) -> impl Iterator<Item = (PathPlan<'_>, &T)> {
    records.iter().map(|(key, record)| (identity(key), record))
}

pub(crate) fn keys<T>(
    records: &std::collections::BTreeMap<String, T>,
) -> impl Iterator<Item = PathPlan<'_>> {
    records.keys().map(identity)
}

pub(crate) fn entries_mut<T>(
    records: &mut std::collections::BTreeMap<String, T>,
) -> impl Iterator<Item = (PathPlan<'_>, &mut T)> {
    records
        .iter_mut()
        .map(|(key, record)| (identity(key), record))
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum WorkspacePlan<'a> {
    Workspace(PathPlan<'a>),
    Fill {
        /// An opaque job key, never a checkout or a mount destination.
        identifier: &'a str,
        checkout: PathPlan<'a>,
    },
}

impl<'a> WorkspacePlan<'a> {
    pub(crate) fn checkout(self) -> PathPlan<'a> {
        match self {
            Self::Workspace(path) | Self::Fill { checkout: path, .. } => path,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RunPlan<'a> {
    pub(crate) workspace: WorkspacePlan<'a>,
    pub(crate) caller_cwd: CallerPath<'a>,
    pub(crate) host_cwd: HostPath<'a>,
    pub(crate) caller_return_paths: &'a Vec<String>,
}

pub(crate) fn run(request: &Run) -> RunPlan<'_> {
    RunPlan {
        workspace: if request.workspace_key.starts_with("base:") {
            WorkspacePlan::Fill {
                identifier: &request.workspace_key,
                checkout: identity(&request.cwd),
            }
        } else {
            WorkspacePlan::Workspace(identity(&request.workspace_key))
        },
        caller_cwd: CallerPath(&request.cwd),
        host_cwd: HostPath(Path::new(&request.cwd)),
        caller_return_paths: &request.return_paths,
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RequestPlan<'a> {
    pub(crate) caller_workspace_key: CallerPath<'a>,
    pub(crate) host_workspace: HostPath<'a>,
    pub(crate) caller_repository_root: CallerPath<'a>,
    pub(crate) host_repository_root: HostPath<'a>,
}

impl<'a> RequestPlan<'a> {
    pub(crate) fn workspace_path(self) -> PathPlan<'a> {
        PathPlan {
            caller: self.caller_workspace_key,
            host: self.host_workspace,
        }
    }
    pub(crate) fn repository_path(self) -> PathPlan<'a> {
        PathPlan {
            caller: self.caller_repository_root,
            host: self.host_repository_root,
        }
    }
}

pub(crate) trait Request<'a> {
    fn paths(self) -> RequestPlan<'a>;
}

pub(crate) fn request<'a>(value: impl Request<'a>) -> RequestPlan<'a> {
    value.paths()
}

impl<'a> Request<'a> for &&'a Run {
    fn paths(self) -> RequestPlan<'a> {
        (*self).paths()
    }
}

impl<'a> Request<'a> for &&'a Prepare {
    fn paths(self) -> RequestPlan<'a> {
        (*self).paths()
    }
}

impl<'a> Request<'a> for &'a Prepare {
    fn paths(self) -> RequestPlan<'a> {
        RequestPlan {
            caller_workspace_key: CallerPath(&self.workspace_key),
            host_workspace: HostPath(Path::new(&self.workspace_key)),
            caller_repository_root: CallerPath(&self.repository_root),
            host_repository_root: HostPath(Path::new(&self.repository_root)),
        }
    }
}

impl<'a> Request<'a> for &'a Run {
    fn paths(self) -> RequestPlan<'a> {
        RequestPlan {
            caller_workspace_key: CallerPath(&self.workspace_key),
            host_workspace: run(self).workspace.checkout().host,
            caller_repository_root: CallerPath(&self.repository_root),
            host_repository_root: HostPath(Path::new(&self.repository_root)),
        }
    }
}

pub(crate) fn sibling(sibling: &SiblingSnapshot) -> PathPlan<'_> {
    identity(&sibling.canonical_path)
}

pub(crate) fn drop_request(request: &motor_protocol::DropRequest) -> PathPlan<'_> {
    identity(&request.workspace_key)
}

pub(crate) fn job_key(job: &Job) -> &str {
    match job.request.as_ref() {
        Some(request) => match run(request).workspace {
            WorkspacePlan::Fill { identifier, .. } => identifier,
            WorkspacePlan::Workspace(path) => path.caller.text(),
        },
        None => "",
    }
}

pub(crate) fn job_workspace(job: &Job) -> HostPath<'_> {
    job.request
        .as_ref()
        .map(|r| request(r).host_workspace)
        .unwrap_or_else(|| host_str(""))
}

pub(crate) struct RecordPlan<'a> {
    pub(crate) repository: PathPlan<'a>,
    pub(crate) caller_siblings: &'a [SiblingSnapshot],
}

pub(crate) fn workspace_record(record: &Workspace) -> RecordPlan<'_> {
    RecordPlan {
        repository: identity(&record.repository),
        caller_siblings: &record.siblings,
    }
}

pub(crate) fn retained_transfer(record: &RetainedTransfer) -> RecordPlan<'_> {
    RecordPlan {
        repository: identity(&record.repository),
        caller_siblings: &record.siblings,
    }
}

pub(crate) fn legacy_checkout(record: &LegacyCheckout) -> PathPlan<'_> {
    identity(&record.repository)
}

pub(crate) fn base(record: &BaseRequest) -> PathPlan<'_> {
    identity(&record.workspace)
}

/// Each record pins only the mappings referenced by its own paths.
pub(crate) fn mappings<'a>(
    roots: &[CallerRoot],
    paths: impl IntoIterator<Item = &'a str>,
) -> Vec<CallerRoot> {
    let paths: Vec<_> = paths.into_iter().map(Path::new).collect();
    roots
        .iter()
        .filter(|root| {
            paths.iter().any(|path| {
                *path != Path::new(&root.caller_prefix) && path.starts_with(&root.caller_prefix)
            })
        })
        .cloned()
        .collect()
}

pub(crate) fn job_mappings(job: &Job, roots: &[CallerRoot]) -> Vec<CallerRoot> {
    let Some(request) = &job.request else {
        return vec![];
    };
    mappings(
        roots,
        [
            request.workspace_key.as_str(),
            request.repository_root.as_str(),
            request.cwd.as_str(),
        ]
        .into_iter()
        .chain(request.siblings.iter().map(|s| s.canonical_path.as_str())),
    )
}

pub(crate) fn mapped_root<'a>(config: &'a crate::Config, value: &str) -> Option<&'a CallerRoot> {
    config
        .caller_roots
        .iter()
        .find(|root| Path::new(value).starts_with(&root.caller_prefix))
}

pub(crate) fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn conflict(value: &str, other: impl std::fmt::Display, why: &str) -> io::Error {
    io::Error::other(format!("caller path {value} conflicts with {other}: {why}"))
}

/// Resolve a catalog path without looking at the caller-form filesystem node.
/// Authorization must precede use; this conversion alone grants no access.
pub(crate) fn host_path(config: &crate::Config, value: &str) -> PathBuf {
    match mapped_root(config, value) {
        Some(root) => root
            .host_root
            .join(Path::new(value).strip_prefix(&root.caller_prefix).unwrap()),
        None => PathBuf::from(value),
    }
}

pub(crate) fn collision(value: &str, reserved: &[PathBuf]) -> io::Result<()> {
    if let Some(other) = reserved
        .iter()
        .find(|other| overlaps(Path::new(value), other))
    {
        return Err(conflict(
            value,
            other.display(),
            "runner-owned sandbox destination",
        ));
    }
    Ok(())
}

fn reserved_destinations(config: &crate::Config) -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        crate::isolation::reserved_destinations(config)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        vec![]
    }
}

fn check_no_symlinks(root: &CallerRoot, value: &str, host: &Path) -> io::Result<()> {
    // Pin every existing ancestor before inspecting the next component. O_PATH
    // permits inspection without reading a job-owned directory or link target.
    #[cfg(target_os = "linux")]
    {
        use std::os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        };
        let mut directory = fs::File::open("/")?;
        for component in host.components().skip(1) {
            let name = std::ffi::CString::new(component.as_os_str().as_bytes())
                .map_err(|_| conflict(value, &root.caller_prefix, "invalid mapped component"))?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if fd < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
                    return Ok(());
                }
                return Err(conflict(
                    value,
                    &root.caller_prefix,
                    "cannot inspect mapped destination",
                ));
            }
            directory = unsafe { fs::File::from_raw_fd(fd) };
            let metadata = directory.metadata()?;
            if metadata.is_symlink() {
                return Err(conflict(
                    value,
                    &root.caller_prefix,
                    "symlink in mapped destination",
                ));
            }
            if !metadata.is_dir() {
                return Err(conflict(
                    value,
                    &root.caller_prefix,
                    "mapped destination is not a directory",
                ));
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = host;
        Err(conflict(
            value,
            &root.caller_prefix,
            "mapped destinations require Linux",
        ))
    }
}

/// `fill` is reserved for runner-derived checkout paths, never wire fields.
pub(crate) fn authorize(config: &crate::Config, value: &str, fill: bool) -> io::Result<PathBuf> {
    if config.caller_roots.is_empty() {
        crate::workspace::validate_destinations(
            &config.owned_workspace_roots,
            host_str(value),
            None,
            &[],
        )?;
        return Ok(PathBuf::from(value));
    }
    collision(value, &reserved_destinations(config))?;
    if let Some(root) = mapped_root(config, value) {
        let relative = Path::new(value).strip_prefix(&root.caller_prefix).unwrap();
        if relative.as_os_str().is_empty() || !startup::canonical_spelling(value) {
            return Err(conflict(
                value,
                &root.caller_prefix,
                "expected a canonical path strictly beneath the prefix",
            ));
        }
        if !fill
            && relative
                .components()
                .next()
                .is_some_and(|part| part.as_os_str().to_string_lossy().starts_with(".motor-"))
        {
            return Err(conflict(
                value,
                &root.caller_prefix,
                "runner-reserved name beneath caller prefix",
            ));
        }
        let fill_prefix = format!(".motor-base-{}-", config.instance);
        if !fill
            && relative
                .components()
                .any(|part| part.as_os_str().to_string_lossy().starts_with(&fill_prefix))
        {
            return Err(conflict(
                value,
                &root.caller_prefix,
                "runner-derived fill checkout",
            ));
        }
        let host = root.host_root.join(relative);
        check_no_symlinks(root, value, &host)?;
        return Ok(host);
    }
    for root in &config.caller_roots {
        if Path::new(value).starts_with(&root.host_root) {
            return Err(conflict(
                value,
                &root.caller_prefix,
                "host-form caller path",
            ));
        }
    }
    if !crate::workspace::absolute(value) {
        return Err(io::Error::other("invalid workspace path"));
    }
    let resolved = crate::workspace::destination(Path::new(value))?;
    if let Some(root) = config
        .caller_roots
        .iter()
        .find(|root| resolved.starts_with(&root.host_root))
    {
        return Err(conflict(
            value,
            &root.caller_prefix,
            "identity path resolves into a mapped namespace",
        ));
    }
    crate::workspace::validate_destinations(
        &config.owned_workspace_roots,
        host_str(value),
        None,
        &[],
    )?;
    Ok(PathBuf::from(value))
}

pub(crate) fn fill_checkout(
    config: &crate::Config,
    workspace: &str,
    repository: &str,
) -> io::Result<PathBuf> {
    Ok(Path::new(workspace)
        .parent()
        .ok_or_else(|| io::Error::other("workspace parent"))?
        .join(format!(
            ".motor-base-{}-{}",
            config.instance,
            crate::workspace::name(repository)
        )))
}

pub(crate) struct AuthorizedRequest {
    pub(crate) host_workspace: PathBuf,
    pub(crate) host_cwd: Option<PathBuf>,
    pub(crate) host_siblings: Vec<SiblingSnapshot>,
}

/// Alias links in an identity parent may name a mapped sibling literally.
/// Resolving that target would inspect the host's unrelated caller namespace.
pub(crate) fn aliases(
    config: &crate::Config,
    workspace: &str,
    siblings: &[SiblingSnapshot],
) -> io::Result<Vec<crate::workspace::SiblingAlias>> {
    let mut aliases = Vec::new();
    for sibling in siblings {
        let target = self::sibling(sibling).caller.text();
        let path = Path::new(workspace)
            .parent()
            .ok_or_else(|| io::Error::other("workspace parent"))?
            .join(
                Path::new(target)
                    .file_name()
                    .ok_or_else(|| io::Error::other("sibling name"))?,
            );
        let caller = path
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF8 alias"))?;
        if mapped_root(config, workspace).is_some() {
            let host = authorize(config, caller, false)?;
            aliases.push(crate::workspace::SiblingAlias {
                path: host.clone(),
                canonical: host,
            });
        } else if mapped_root(config, target).is_some() {
            collision(caller, &reserved_destinations(config))?;
            let parent = path.parent().unwrap();
            let canonical = crate::workspace::destination(parent)?.join(path.file_name().unwrap());
            let virtual_node = crate::workspace::SiblingAlias {
                path: path.clone(),
                canonical,
            };
            crate::workspace::validate_alias_destinations(
                &config.owned_workspace_roots,
                std::slice::from_ref(&virtual_node),
            )?;
            if config
                .caller_roots
                .iter()
                .any(|root| virtual_node.canonical.starts_with(&root.host_root))
            {
                return Err(conflict(
                    caller,
                    target,
                    "alias parent resolves into a mapped namespace",
                ));
            }
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_symlink() => {
                    if fs::read_link(&path)?.as_os_str() != std::ffi::OsStr::new(target) {
                        return Err(conflict(
                            caller,
                            target,
                            "alias link must name the caller sibling literally",
                        ));
                    }
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(conflict(caller, target, "alias node is not a directory"))
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            aliases.push(virtual_node);
        } else {
            let one = crate::workspace::sibling_aliases(
                host_str(workspace),
                std::slice::from_ref(sibling),
            )?;
            for alias in &one {
                authorize(config, &alias.path.to_string_lossy(), false)?;
            }
            aliases.extend(one);
        }
    }
    Ok(aliases)
}

pub(crate) fn authorize_request(
    config: &crate::Config,
    plan: RequestPlan<'_>,
    cwd: Option<CallerPath<'_>>,
    siblings: &[SiblingSnapshot],
) -> io::Result<AuthorizedRequest> {
    let workspace = plan.caller_workspace_key.text();
    let repository = plan.caller_repository_root.text();
    let mapped = mapped_root(config, workspace).is_some()
        || siblings
            .iter()
            .any(|s| mapped_root(config, self::sibling(s).caller.text()).is_some());
    if !config.isolate_jobs && mapped && !siblings.is_empty() {
        return Err(conflict(
            workspace,
            siblings
                .first()
                .map(|s| self::sibling(s).caller.text().as_str())
                .unwrap_or(repository),
            "mapped siblings require isolation",
        ));
    }
    let host_workspace = authorize(config, workspace, false)?;
    let host_repository = authorize(config, repository, false)?;
    let host_cwd = cwd
        .map(|cwd| {
            let host = authorize(config, cwd.text(), false)?;
            if !Path::new(cwd.text()).starts_with(workspace) {
                return Err(conflict(cwd.text(), workspace, "cwd outside workspace"));
            }
            Ok(host)
        })
        .transpose()?;
    let mut host_siblings = Vec::new();
    for sibling in siblings {
        let mut sibling = sibling.clone();
        sibling.canonical_path = authorize(config, &sibling.canonical_path, false)?
            .to_string_lossy()
            .into_owned();
        host_siblings.push(sibling);
    }
    crate::workspace::validate_names(
        workspace,
        repository,
        siblings,
        config.sibling_snapshot_mode(),
    )?;
    let aliases = if config.sibling_snapshot_mode() {
        aliases(config, workspace, siblings)?
    } else {
        vec![]
    };
    if config.sibling_snapshot_mode() {
        let mut mounts = vec![
            crate::workspace::destination(&host_workspace)?,
            crate::workspace::destination(&host_repository)?,
        ];
        for (sibling, alias) in host_siblings.iter().zip(&aliases) {
            let sibling = crate::workspace::destination(Path::new(&sibling.canonical_path))?;
            for mount in [&sibling, &alias.canonical] {
                if mounts.iter().any(|other| overlaps(mount, other)) {
                    return Err(io::Error::other("sibling or its worktree alias overlaps workspace, repository or another sibling"));
                }
            }
            mounts.extend([sibling, alias.canonical.clone()]);
        }
    }
    Ok(AuthorizedRequest {
        host_workspace,
        host_cwd,
        host_siblings,
    })
}

fn caller_from_host(config: &crate::Config, host: &Path) -> String {
    for root in &config.caller_roots {
        if let Ok(relative) = host.strip_prefix(&root.host_root) {
            return Path::new(&root.caller_prefix)
                .join(relative)
                .to_string_lossy()
                .into_owned();
        }
    }
    host.to_string_lossy().into_owned()
}

pub(crate) fn host_aliases(
    config: &crate::Config,
    workspace: HostPath<'_>,
    siblings: &[SiblingSnapshot],
) -> io::Result<Vec<crate::workspace::SiblingAlias>> {
    let caller_workspace = caller_from_host(config, workspace.as_path());
    let caller_siblings: Vec<_> = siblings
        .iter()
        .cloned()
        .map(|mut sibling| {
            sibling.canonical_path = caller_from_host(config, Path::new(&sibling.canonical_path));
            sibling
        })
        .collect();
    aliases(config, &caller_workspace, &caller_siblings)
}

pub(crate) fn validate_host_destinations(
    config: &crate::Config,
    workspace: HostPath<'_>,
    cwd: Option<HostPath<'_>>,
    siblings: &[SiblingSnapshot],
) -> io::Result<()> {
    let validate = |host: HostPath<'_>| -> io::Result<()> {
        let value = host.as_path().to_string_lossy();
        if let Some(root) = config
            .caller_roots
            .iter()
            .find(|root| host.as_path().starts_with(&root.host_root))
        {
            let caller = caller_from_host(config, host.as_path());
            if host.as_path() == root.host_root || !startup::canonical_spelling(&value) {
                return Err(conflict(
                    &caller,
                    &root.caller_prefix,
                    "expected a path strictly beneath the prefix",
                ));
            }
            collision(&caller, &reserved_destinations(config))?;
            check_no_symlinks(root, &caller, host.as_path())
        } else if let Some(root) = mapped_root(config, &value) {
            Err(conflict(
                &value,
                &root.caller_prefix,
                "untranslated caller path in host operation",
            ))
        } else {
            authorize(config, &value, false).map(|_| ())
        }
    };
    validate(workspace)?;
    if let Some(cwd) = cwd {
        validate(cwd)?;
        if !cwd.as_path().starts_with(workspace.as_path()) {
            return Err(io::Error::other("cwd outside workspace"));
        }
    }
    for sibling in siblings {
        validate(self::sibling(sibling).host)?;
    }
    if config.sibling_snapshot_mode() {
        host_aliases(config, workspace, siblings)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_preserves_spelling_without_filesystem_access() {
        for text in ["/Users/u/./repo", "/Users//u/repo", "/Users/u/repo/"] {
            let value = text.to_owned();
            let plan = identity(&value);
            assert_eq!(plan.caller.text(), text);
            assert_eq!(plan.host.as_path().as_os_str(), text);
        }
    }

    #[test]
    fn fill_identifiers_never_become_checkout_paths() {
        let job: Job = serde_json::from_str(include_str!(
            "../test-fixtures/compatibility/job-record.json"
        ))
        .unwrap();
        let mut wire = job.request.unwrap();
        for (repository, checkout) in [
            ("/home/repo", "/home/.motor-base-main-repo"),
            ("/home/u/repo", "/home/u/.motor-base-main-repo"),
        ] {
            wire.repository_root = repository.into();
            wire.workspace_key = format!("base:{repository}");
            wire.cwd = checkout.into();
            let plan = run(&wire);
            match plan.workspace {
                WorkspacePlan::Fill {
                    identifier,
                    checkout: path,
                } => {
                    assert_eq!(identifier, format!("base:{repository}"));
                    assert_eq!(path.caller.text(), checkout);
                    assert_eq!(path.host.as_path(), Path::new(checkout));
                }
                WorkspacePlan::Workspace(_) => panic!("fill identifier became a workspace path"),
            }
            assert_eq!(request(&wire).host_workspace.as_path(), Path::new(checkout));
        }
    }
}
