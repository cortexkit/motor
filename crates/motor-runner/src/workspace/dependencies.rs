use super::*;
use std::io::{BufRead, BufReader};

fn valid_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
/// Shared Git source identifier check for Linux, Windows and macOS prep.
/// The original URL is only an identifier that Cargo rewrites to a local
/// mirror, so it must name a supported scheme with a host. A username such
/// as `git@` is allowed, but a password must never reach the job.
pub(crate) fn valid_url(url: &str) -> bool {
    !url.chars().any(char::is_control)
        && url::Url::parse(url).is_ok_and(|u| {
            matches!(u.scheme(), "https" | "http" | "ssh" | "git")
                && u.host_str().is_some_and(|h| !h.is_empty())
                && u.password().is_none()
        })
}

pub(crate) fn validate_dependencies(dependencies: &[GitDependency]) -> io::Result<()> {
    let mut bundled = BTreeSet::new();
    for d in dependencies {
        if !valid_url(&d.url) || d.commits.is_empty() || d.commits.iter().any(|c| !valid_commit(c))
        {
            return Err(invalid("invalid git dependency URL or commit"));
        }
        if let Some(bundle) = &d.bundle {
            if d.commits.len() != 1 || bundle.repository != d.url || !bundled.insert(&d.commits[0])
            {
                return Err(invalid(
                    "git dependency bundle must declare one unique commit",
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn dependency_commits(
    dependencies: &[GitDependency],
) -> BTreeMap<String, BTreeSet<String>> {
    let mut grouped: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for d in dependencies {
        grouped
            .entry(d.url.clone())
            .or_default()
            .extend(d.commits.iter().cloned());
    }
    grouped
}

#[cfg(target_os = "linux")]
fn snapshot_lockfiles(repository: &Path, commit: &str) -> io::Result<Vec<String>> {
    let entries = git(repository, &["ls-tree", "-rz", commit])?;
    let files: BTreeMap<_, _> = entries
        .split('\0')
        .filter_map(|entry| {
            let (header, path) = entry.split_once('\t')?;
            Some((
                path,
                header.starts_with("100644 blob ") || header.starts_with("100755 blob "),
            ))
        })
        .collect();
    let mut locks =
        vec![git(repository, &["show", &format!("{commit}:Cargo.lock")]).unwrap_or_default()];
    let mut count = usize::from(files.contains_key("Cargo.lock"));
    for (path, regular) in &files {
        let path = Path::new(path);
        if path == Path::new("Cargo.lock")
            || path.file_name().is_none_or(|name| name != "Cargo.lock")
        {
            continue;
        }
        count += 1;
        if count > 128
            || !regular
            || files.get(path.with_file_name("Cargo.toml").to_str().unwrap_or("")) != Some(&true)
        {
            continue;
        }
        let lock = git(
            repository,
            &["show", &format!("{commit}:{}", path.display())],
        )?;
        // Nested graphs are optional prefetches; a malformed experimental lock
        // must not make mirror route selection fail.
        if locked_dependencies(&lock).is_ok() {
            locks.push(lock);
        }
    }
    Ok(locks)
}

#[cfg(target_os = "linux")]
pub(crate) fn fetch_context(workspace: &Path, routes: &BTreeMap<String, PathBuf>) -> String {
    fn manifest_sources(value: &toml::Value, sources: &mut BTreeSet<(String, String)>) {
        if let Some(table) = value.as_table() {
            if let Some(url) = table
                .get("git")
                .and_then(toml::Value::as_str)
                .filter(|url| valid_url(url))
            {
                let selector = ["rev", "branch", "tag"]
                    .into_iter()
                    .find_map(|key| {
                        table
                            .get(key)
                            .and_then(toml::Value::as_str)
                            .map(|v| format!("{key} {v}"))
                    })
                    .unwrap_or_else(|| "default branch".into());
                sources.insert((url.to_owned(), selector));
            }
            for child in table.values() {
                manifest_sources(child, sources);
            }
        }
    }
    let mut sources = BTreeSet::new();
    if let Ok(manifest) = fs::read_to_string(workspace.join("Cargo.toml")) {
        if let Ok(value) = toml::from_str(&manifest) {
            manifest_sources(&value, &mut sources);
        }
    }
    if let Ok(lock) = fs::read_to_string(workspace.join("Cargo.lock")) {
        for (url, commit, _) in locked_sources(&lock).unwrap_or_default() {
            sources.insert((url, format!("locked commit {commit}")));
        }
    }
    sources
        .into_iter()
        .map(|(url, selector)| {
            let transport = match routes.get(&url) {
                Some(mirror) => format!("routed to local mirror {}", mirror.display()),
                None => "upstream; no local mirror rewrite".into(),
            };
            format!("git dependency {url} {selector} ({transport})")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

impl Trees {
    #[cfg(target_os = "linux")]
    pub(crate) fn checkout_dependency_lockfiles(
        &self,
        repository: &str,
        commit: &str,
        siblings: &[SiblingSnapshot],
    ) -> Vec<String> {
        std::iter::once((repository, commit))
            .chain(siblings.iter().map(|s| {
                (
                    crate::caller_path::sibling(s).caller.text().as_str(),
                    s.commit.as_str(),
                )
            }))
            .flat_map(|(repository, commit)| {
                snapshot_lockfiles(&self.mirror(repository), commit).unwrap_or_default()
            })
            .collect()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn dependency_fetch_routes_for_lockfiles(
        &self,
        dependencies: &[GitDependency],
        lockfiles: &[String],
    ) -> io::Result<(BTreeMap<String, PathBuf>, Vec<String>)> {
        let mut declarations = dependencies.to_vec();
        for lockfile in lockfiles {
            for dependency in locked_dependencies(lockfile)? {
                if dependencies.iter().any(|d| d.url == dependency.url) {
                    declarations.push(dependency);
                }
            }
        }
        self.dependency_routes(&declarations)
    }

    pub fn dependency_mirror(&self, url: &str) -> PathBuf {
        self.state
            .join("git-deps")
            .join(format!("{}.git", name(url)))
    }
    pub fn dependency_lock(&self, mirror: &Path) -> Arc<Mutex<()>> {
        self.mirror_lock(mirror)
    }
    pub fn have_dependencies(&self, urls: Vec<String>) -> io::Result<Vec<GitDependencyCandidates>> {
        let mut reply = Vec::new();
        for url in urls.into_iter().collect::<BTreeSet<_>>() {
            if !valid_url(&url) {
                return Err(invalid("invalid git dependency URL"));
            }
            let mirror = self.dependency_mirror(&url);
            let lock = self.dependency_lock(&mirror);
            let _guard = crate::setup_log::lock_wait("dependency_mirror", || lock.lock().unwrap());
            let listed = git(
                &mirror,
                &[
                    "for-each-ref",
                    "--format=%(objectname) %(refname)",
                    "refs/ck-motor/dep/",
                ],
            )
            .unwrap_or_default();
            let commits = listed
                .lines()
                .filter_map(|line| line.split_once(' '))
                .filter(|(oid, reference)| {
                    valid_commit(oid)
                        && *reference == format!("refs/ck-motor/dep/{oid}")
                        && git(&mirror, &["cat-file", "-t", oid]).is_ok_and(|t| t == "commit")
                })
                .map(|(oid, _)| oid.to_owned())
                .collect();
            reply.push(GitDependencyCandidates { url, commits });
        }
        Ok(reply)
    }

    /// Validate every dependency bundle in an empty repository before any mirror
    /// import. A dependency bundle must carry its full history, not require objects
    /// from an earlier job's commit; the empty repository detects such prerequisites.
    pub fn validate_dependency_bundles(
        &self,
        directory: &Path,
        dependencies: &[GitDependency],
    ) -> io::Result<()> {
        validate_dependencies(dependencies)?;
        for d in dependencies {
            let Some(b) = &d.bundle else { continue };
            let spool = directory.join(b.index.to_string());
            let mut header = BufReader::new(fs::File::open(&spool)?);
            let mut line = String::new();
            loop {
                line.clear();
                if header.read_line(&mut line)? == 0 {
                    return Err(invalid("unterminated dependency bundle header"));
                }
                if line.starts_with('-') {
                    return Err(invalid("dependency bundle has prerequisites"));
                }
                if line.trim_end().is_empty() {
                    break;
                }
            }
            let reference = format!("refs/ck-motor/dep/{}", d.commits[0]);
            let expected = format!("{} {reference}", d.commits[0]);
            let validation = directory.join(format!("validate-dependency-{}", b.index));
            fs::create_dir_all(&validation)?;
            git(
                &validation,
                &[
                    "init",
                    "--bare",
                    if d.commits[0].len() == 64 {
                        "--object-format=sha256"
                    } else {
                        "--object-format=sha1"
                    },
                ],
            )?;
            let spool = spool.to_str().ok_or_else(|| invalid("bundle path"))?;
            let heads = git(&validation, &["bundle", "list-heads", spool])?;
            if heads != expected {
                return Err(invalid(
                    "dependency bundle must have exactly its declared ref",
                ));
            }
            git(&validation, &["bundle", "verify", spool])?;
            super::transfer::fetch(
                &validation,
                &[
                    "-c",
                    "gc.auto=0",
                    "fetch",
                    "--progress",
                    "--no-auto-maintenance",
                    "--no-tags",
                    spool,
                    &format!("{reference}:{reference}"),
                ],
            )?;
            if git(&validation, &["cat-file", "-t", &d.commits[0]])? != "commit" {
                return Err(invalid("dependency object is not a commit"));
            }
        }
        Ok(())
    }

    pub fn import_dependencies(
        &self,
        directory: &Path,
        dependencies: &[GitDependency],
    ) -> io::Result<()> {
        for d in dependencies {
            let Some(b) = &d.bundle else { continue };
            let mirror = self.dependency_mirror(&d.url);
            let lock = self.dependency_lock(&mirror);
            let _guard = crate::setup_log::lock_wait("dependency_mirror", || lock.lock().unwrap());
            if !mirror.exists() {
                fs::create_dir_all(&mirror)?;
                git(
                    &mirror,
                    &[
                        "init",
                        "--bare",
                        if d.commits[0].len() == 64 {
                            "--object-format=sha256"
                        } else {
                            "--object-format=sha1"
                        },
                    ],
                )?;
                git(&mirror, &["config", "gc.auto", "0"])?;
                git(&mirror, &["config", "maintenance.auto", "false"])?;
            }
            let spool = directory.join(b.index.to_string());
            let reference = format!("refs/ck-motor/dep/{}", d.commits[0]);
            super::transfer::fetch(
                &mirror,
                &[
                    "-c",
                    "gc.auto=0",
                    "fetch",
                    "--progress",
                    "--no-auto-maintenance",
                    "--no-tags",
                    "--no-write-fetch-head",
                    spool.to_str().ok_or_else(|| invalid("bundle path"))?,
                    &format!("{reference}:{reference}"),
                ],
            )?;
        }
        Ok(())
    }

    pub fn dependency_routes(
        &self,
        dependencies: &[GitDependency],
    ) -> io::Result<(BTreeMap<String, PathBuf>, Vec<String>)> {
        self.dependency_routes_checked(dependencies, None)
    }
    fn dependency_routes_checked(
        &self,
        dependencies: &[GitDependency],
        check: Option<&dyn Fn() -> io::Result<()>>,
    ) -> io::Result<(BTreeMap<String, PathBuf>, Vec<String>)> {
        validate_dependencies(dependencies)?;
        let grouped = dependency_commits(dependencies);
        let mut routes = BTreeMap::new();
        let mut missing = Vec::new();
        for (url, commits) in &grouped {
            if let Some(check) = check {
                check()?;
            }
            let mirror = self.dependency_mirror(url);
            let lock = self.dependency_lock(&mirror);
            let _guard = if let Some(check) = check {
                crate::setup_log::lock_wait("dependency_mirror", || loop {
                    check()?;
                    match lock.try_lock() {
                        Ok(guard) => break Ok(guard),
                        Err(std::sync::TryLockError::WouldBlock) => {
                            std::thread::park_timeout(std::time::Duration::from_millis(10))
                        }
                        Err(e) => break Err(invalid(e)),
                    }
                })?
            } else {
                crate::setup_log::lock_wait("dependency_mirror", || lock.lock().unwrap())
            };
            let absent: Vec<_> = commits
                .iter()
                .filter(|c| {
                    !git(
                        &mirror,
                        &["rev-parse", "--verify", &format!("refs/ck-motor/dep/{c}")],
                    )
                    .is_ok_and(|oid| oid == **c)
                        || !git(&mirror, &["cat-file", "-t", c]).is_ok_and(|t| t == "commit")
                })
                .collect();
            missing.extend(
                absent
                    .iter()
                    .map(|c| format!("git dependency {url} missing commit {c}")),
            );
            if absent.is_empty()
                && !grouped
                    .keys()
                    .any(|other| other != url && other.starts_with(url))
            {
                routes.insert(url.clone(), mirror);
            }
        }
        Ok((routes, missing))
    }
}

/// Construct Git's transport settings only for the trusted child. No account
/// configuration or inherited Git variable may supply a credential or a rewrite.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn fetch_command(
    state: &Path,
    workspace: &Path,
    routes: &BTreeMap<String, PathBuf>,
    cargo: &Path,
    rustup: &Path,
) -> io::Result<Command> {
    let empty_home = state.join("fetch-home");
    fs::create_dir_all(&empty_home)?;
    let mut cmd = Command::new("cargo");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New cache entries must have the same owner as startup-validated
        // entries, including when this command runs outside a snapshot helper.
        cmd.uid(unsafe { libc::geteuid() });
    }
    cmd.args(["fetch", "--locked", "--manifest-path"])
        .arg(workspace.join("Cargo.toml"))
        .current_dir(state);
    if !routes.is_empty() {
        cmd.args(["--config", "net.git-fetch-with-cli=true"]);
    }
    cmd.env("CARGO_HOME", cargo)
        .env("RUSTUP_HOME", rustup)
        // The archive lower is append-only for its lifetime: Cargo may fill a
        // new/empty .crate, but must not age-delete a published archive. Index,
        // source trees and Git are private job state, not live lowers. Manual
        // archive repair/cleanup must drain and retire the lower first.
        .env("CARGO_CACHE_AUTO_CLEAN_FREQUENCY", "never")
        // Empty overrides also disable wrappers configured in CARGO_HOME. Merely
        // unsetting these variables lets Cargo resurrect the shared sccache.
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "");
    configure_fetch_git(&mut cmd, &empty_home, routes)?;
    Ok(cmd)
}

#[cfg(any(unix, test))]
pub(crate) fn configure_fetch_git(
    cmd: &mut Command,
    empty_home: &Path,
    routes: &BTreeMap<String, PathBuf>,
) -> io::Result<()> {
    let inherited: BTreeSet<_> = std::env::vars_os()
        .map(|(k, _)| k)
        .chain(cmd.get_envs().map(|(k, _)| k.to_owned()))
        .collect();
    for key in inherited {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("HOME", empty_home)
        .env("XDG_CONFIG_HOME", empty_home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ALLOW_PROTOCOL", "file:https:http")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", routes.len().to_string());
    for (i, (url, mirror)) in routes.iter().enumerate() {
        if !valid_url(url) {
            return Err(invalid("invalid git dependency URL"));
        }
        let transport =
            url::Url::from_directory_path(mirror).map_err(|_| invalid("dependency mirror path"))?;
        cmd.env(
            format!("GIT_CONFIG_KEY_{i}"),
            format!("url.{transport}.insteadOf"),
        )
        .env(format!("GIT_CONFIG_VALUE_{i}"), url);
    }
    Ok(())
}

fn locked_sources(lockfile: &str) -> io::Result<Vec<(String, String, Option<String>)>> {
    if lockfile.is_empty() {
        return Ok(Vec::new());
    }
    let lock: toml::Value = toml::from_str(lockfile).map_err(invalid)?;
    let mut sources = Vec::new();
    for package in lock
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(source) = package
            .get("source")
            .and_then(toml::Value::as_str)
            .and_then(|s| s.strip_prefix("git+"))
        else {
            continue;
        };
        let (fetch, commit) = source
            .rsplit_once('#')
            .ok_or_else(|| invalid("git dependency lacks locked commit"))?;
        let url = fetch.split('?').next().unwrap().to_owned();
        if !valid_url(&url) || !valid_commit(commit) {
            return Err(invalid("invalid locked git source"));
        }
        let parsed = url::Url::parse(fetch).map_err(invalid)?;
        let queries: Vec<_> = parsed.query_pairs().collect();
        let alias = if queries.iter().any(|(k, _)| k == "rev") {
            None
        } else if let Some((_, branch)) = queries.iter().find(|(k, _)| k == "branch") {
            Some(format!("refs/heads/{branch}"))
        } else if let Some((_, tag)) = queries.iter().find(|(k, _)| k == "tag") {
            Some(format!("refs/tags/{tag}"))
        } else {
            Some("refs/heads/ck-motor-default".into())
        };
        sources.push((url, commit.to_owned(), alias));
    }
    Ok(sources)
}

pub(crate) fn locked_dependencies(lockfile: &str) -> io::Result<Vec<GitDependency>> {
    Ok(locked_sources(lockfile)?
        .into_iter()
        .map(|(url, commit, _)| GitDependency {
            url,
            commits: vec![commit],
            bundle: None,
        })
        .collect())
}

impl Trees {
    pub fn dependency_fetch_routes(
        &self,
        dependencies: &[GitDependency],
        lockfile: &str,
    ) -> io::Result<(BTreeMap<String, PathBuf>, Vec<String>)> {
        self.dependency_fetch_routes_checked(dependencies, lockfile, None)
    }
    pub(crate) fn dependency_fetch_routes_checked(
        &self,
        dependencies: &[GitDependency],
        lockfile: &str,
        check: Option<&dyn Fn() -> io::Result<()>>,
    ) -> io::Result<(BTreeMap<String, PathBuf>, Vec<String>)> {
        let mut declarations = dependencies.to_vec();
        for (url, commit, _) in locked_sources(lockfile)? {
            if dependencies.iter().any(|d| d.url == url) {
                declarations.push(GitDependency {
                    url,
                    commits: vec![commit],
                    bundle: None,
                });
            }
        }
        self.dependency_routes_checked(&declarations, check)
    }

    /// The caller holds each mapped mirror's lock until Cargo fetch finishes.
    /// Aliases are derived only from committed lockfiles.
    pub fn refresh_dependency_aliases(
        &self,
        lockfile: &str,
        routes: &BTreeMap<String, PathBuf>,
    ) -> io::Result<()> {
        self.refresh_dependency_aliases_for_lockfiles(&[lockfile], routes)
    }

    pub(crate) fn refresh_dependency_aliases_for_lockfiles(
        &self,
        lockfiles: &[impl AsRef<str>],
        routes: &BTreeMap<String, PathBuf>,
    ) -> io::Result<()> {
        let mut aliases: BTreeMap<(String, String), String> = BTreeMap::new();
        let mut heads: BTreeMap<String, String> = BTreeMap::new();
        let sources = lockfiles
            .iter()
            .map(|lockfile| locked_sources(lockfile.as_ref()))
            .collect::<io::Result<Vec<_>>>()?;
        for (url, commit, alias) in sources.into_iter().flatten() {
            let Some(mirror) = routes.get(&url) else {
                continue;
            };
            if git(
                mirror,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("refs/ck-motor/dep/{commit}"),
                ],
            )? != commit
                || git(mirror, &["cat-file", "-t", &commit])? != "commit"
            {
                return Err(invalid(format!(
                    "git dependency {url} missing commit {commit}"
                )));
            }
            let revision_alias = format!("refs/heads/ck-motor-rev/{commit}");
            if aliases
                .insert((url.clone(), revision_alias.clone()), commit.clone())
                .is_some_and(|previous| previous != commit)
            {
                return Err(invalid(format!(
                    "git dependency {url} conflicting ref {revision_alias}"
                )));
            }
            heads
                .entry(url.clone())
                .and_modify(|head| {
                    if head != "refs/heads/ck-motor-default" && revision_alias < *head {
                        *head = revision_alias.clone();
                    }
                })
                .or_insert(revision_alias);
            if let Some(alias) = alias {
                git(mirror, &["check-ref-format", &alias]).map_err(|e| {
                    invalid(format!("git dependency {url} invalid ref {alias}: {e}"))
                })?;
                if aliases
                    .insert((url.clone(), alias.clone()), commit.clone())
                    .is_some_and(|previous| previous != commit)
                {
                    return Err(invalid(format!(
                        "git dependency {url} conflicting ref {alias}"
                    )));
                }
                if alias == "refs/heads/ck-motor-default" {
                    heads.insert(url, alias);
                }
            }
        }
        for mirror in routes.values() {
            let refs = git(
                mirror,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/heads/",
                    "refs/tags/",
                ],
            )?;
            for reference in refs.lines() {
                git(mirror, &["update-ref", "-d", reference])?;
            }
            crate::filesystem::set_directory_modified(mirror, std::time::SystemTime::now())?;
        }
        for ((url, alias), commit) in aliases {
            let mirror = &routes[&url];
            git(mirror, &["update-ref", &alias, &commit])?;
        }
        // Cargo 1.99 requests HEAD even for an explicit branch, tag or rev.
        // Without a default source, choose the lexicographically smallest locked
        // commit's revision alias so HEAD resolves without depending on input order.
        for (url, alias) in heads {
            git(&routes[&url], &["symbolic-ref", "HEAD", &alias])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod cache_gc_tests {
    use super::*;
    #[test]
    fn trusted_fetch_disables_automatic_shared_cache_cleanup() {
        let scratch = cortexkit_test_support::ScratchDir::new("trusted-fetch-no-gc");
        let command = fetch_command(
            scratch.path(),
            scratch.path(),
            &BTreeMap::new(),
            scratch.path(),
            scratch.path(),
        )
        .unwrap();
        let setting = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("CARGO_CACHE_AUTO_CLEAN_FREQUENCY"))
            .and_then(|(_, value)| value);
        assert_eq!(setting, Some(std::ffi::OsStr::new("never")));
    }
}
