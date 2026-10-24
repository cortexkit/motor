use super::*;
use std::io::{BufRead, BufReader};

fn valid_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_url(url: &str) -> bool {
    !url.chars().any(char::is_control)
        && url::Url::parse(url).is_ok_and(|u| {
            matches!(u.scheme(), "https" | "http" | "ssh" | "git")
                && u.host_str().is_some_and(|h| !h.is_empty())
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

impl Trees {
    pub fn dependency_mirror(&self, url: &str) -> PathBuf {
        self.state
            .join("git-deps")
            .join(format!("{}.git", name(url)))
    }
    pub fn dependency_lock(&self, mirror: &Path) -> Arc<Mutex<()>> {
        self.dependency_locks
            .lock()
            .unwrap()
            .entry(mirror.to_owned())
            .or_default()
            .clone()
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
    cmd.args(["fetch", "--locked", "--manifest-path"])
        .arg(workspace.join("Cargo.toml"))
        .current_dir(state);
    if !routes.is_empty() {
        cmd.args(["--config", "net.git-fetch-with-cli=true"]);
    }
    cmd.env("CARGO_HOME", cargo)
        .env("RUSTUP_HOME", rustup)
        // Empty overrides also disable wrappers configured in CARGO_HOME. Merely
        // unsetting these variables lets Cargo resurrect the shared sccache.
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "");
    configure_fetch_git(&mut cmd, &empty_home, routes)?;
    Ok(cmd)
}

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
    /// Aliases are derived only from the workspace's committed lockfile.
    pub fn refresh_dependency_aliases(
        &self,
        lockfile: &str,
        routes: &BTreeMap<String, PathBuf>,
    ) -> io::Result<()> {
        let mut aliases: BTreeMap<(String, String), String> = BTreeMap::new();
        let mut heads: BTreeMap<String, String> = BTreeMap::new();
        for (url, commit, alias) in locked_sources(lockfile)? {
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
            fs::File::open(mirror)?
                .set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::now()))?;
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
