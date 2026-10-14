use crate::{
    snapshot,
    storage::{Storage, VerificationKey, VerifiedTip},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::Path,
    process::Stdio,
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const REMOTE_TIMEOUT: Duration = Duration::from_secs(5);

struct Failure {
    category: &'static str,
    reason: String,
}

impl Failure {
    fn new(category: &'static str, reason: impl ToString) -> Self {
        Self {
            category,
            reason: reason.to_string(),
        }
    }
}

pub(crate) fn tip(root: &Path, storage: &Storage, scratch: &Path) -> Option<String> {
    tip_with_timeout(root, storage, scratch, REMOTE_TIMEOUT)
}

fn tip_with_timeout(
    root: &Path,
    storage: &Storage,
    scratch: &Path,
    timeout: Duration,
) -> Option<String> {
    tip_with_clock(root, storage, scratch, timeout, Instant::now)
}

fn tip_with_clock(
    root: &Path,
    storage: &Storage,
    scratch: &Path,
    timeout: Duration,
    clock: impl Fn() -> Instant,
) -> Option<String> {
    let started = Instant::now();
    match verify(root, storage, scratch, timeout, &clock) {
        Ok(tip) => tip,
        Err(error) => {
            tracing::warn!(repository = %root.display(), category = error.category,
                reason = %error.reason.lines().next().unwrap_or("remote verification failed"),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "default branch verification failed; cache fill skipped");
            None
        }
    }
}

fn verify(
    root: &Path,
    storage: &Storage,
    scratch: &Path,
    timeout: Duration,
    clock: &impl Fn() -> Instant,
) -> Result<Option<String>, Failure> {
    let common = snapshot::git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map_err(|e| Failure::new("mismatch", e))?;
    let repository = fs::canonicalize(common).map_err(|e| Failure::new("mismatch", e))?;
    let repository = repository
        .to_str()
        .ok_or_else(|| Failure::new("mismatch", "Git common directory is not UTF-8"))?
        .to_owned();
    // Serialize checks for this repository without holding the job-state lock over I/O.
    let check = storage.tip_check(&repository);
    let _check = check.lock().unwrap();
    let Some((branch, commit)) = snapshot::local_default_branch(root) else {
        return Ok(None);
    };
    let remote_url = snapshot::git(root, &["remote", "get-url", "origin"])
        .map_err(|e| Failure::new("network", e))?;
    if storage.verified_tip(&repository).is_some_and(|tip| {
        tip.remote_url == remote_url && tip.branch == branch && tip.commit == commit
    }) {
        return Ok(Some(commit));
    }
    let key = VerificationKey {
        repository: repository.clone(),
        remote_url: remote_url.clone(),
        branch: branch.clone(),
        commit: commit.clone(),
    };
    if storage.tip_failed_recently(&key, clock()) {
        return Ok(None);
    }
    let result = (|| {
        let advertised = remote_head(root, scratch, timeout)?;
        let mut remote_branch = None;
        let mut remote_commit = None;
        for line in advertised.lines() {
            if let Some((value, "HEAD")) = line.split_once('\t') {
                if let Some(reference) = value.strip_prefix("ref: ") {
                    remote_branch = Some(reference);
                } else {
                    remote_commit = Some(value);
                }
            }
        }
        if remote_branch != Some(branch.as_str()) || remote_commit != Some(commit.as_str()) {
            return Err(Failure::new(
                "mismatch",
                "local default branch and commit do not match origin HEAD",
            ));
        }
        if let Err(error) = storage.verify_tip(
            repository,
            VerifiedTip {
                remote_url,
                branch,
                commit: commit.clone(),
                verified_at_ms: 0,
            },
        ) {
            return Err(Failure::new("cache", error));
        }
        Ok(Some(commit))
    })();
    if result.is_err() {
        storage.tip_failed(key, clock());
    }
    result
}

fn read_output(path: &Path) -> io::Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?.take(64 * 1024).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn remote_head(root: &Path, scratch: &Path, timeout: Duration) -> Result<String, Failure> {
    let spool =
        snapshot::Spool::new(scratch, Uuid::now_v7()).map_err(|e| Failure::new("network", e))?;
    let stdout_path = spool.0.join("stdout");
    let stderr_path = spool.0.join("stderr");
    let file = |path: &Path| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    };
    let stdout = file(&stdout_path).map_err(|e| Failure::new("network", e))?;
    let stderr = file(&stderr_path).map_err(|e| Failure::new("network", e))?;
    let mut command = snapshot::command(root);
    command
        .args(["ls-remote", "--symref", "origin", "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_ASKPASS", "/usr/bin/false")
        .env("SSH_ASKPASS", "/usr/bin/false")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0);
    // Files, rather than pipes, let timeout return even if a helper keeps an output fd open.
    let started = Instant::now();
    let mut child = command.spawn().map_err(|e| Failure::new("network", e))?;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(Failure::new("network", error)),
            Ok(None) if started.elapsed() >= timeout => {
                break Err(Failure::new("timeout", "git ls-remote timed out"))
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
        }
    };
    if result.is_err() {
        // Kill Git and its credential/SSH helpers, not only the immediate child.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let stderr = read_output(&stderr_path).unwrap_or_default();
    let first_line = stderr.lines().next().filter(|line| !line.is_empty());
    match result {
        Err(mut error) => {
            if let Some(line) = first_line {
                error.reason = line.to_owned();
            }
            Err(error)
        }
        Ok(status) if !status.success() => {
            let reason = first_line.unwrap_or("git ls-remote failed");
            let lower = reason.to_ascii_lowercase();
            let auth = [
                "authentication",
                "permission denied",
                "username",
                "credential",
                "publickey",
                "terminal prompts disabled",
                "access denied",
            ]
            .iter()
            .any(|needle| lower.contains(needle));
            Err(Failure::new(if auth { "auth" } else { "network" }, reason))
        }
        Ok(_) => read_output(&stdout_path).map_err(|e| Failure::new("network", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

    struct Fixture {
        _scratch: ScratchDir,
        root: PathBuf,
        remote: PathBuf,
        calls: PathBuf,
        upload: PathBuf,
        spool: PathBuf,
        storage: Storage,
    }

    impl Fixture {
        fn new() -> Self {
            let scratch = ScratchDir::new("motor-verified-tip");
            let root = scratch.join("checkout");
            let remote = scratch.join("remote.git");
            let calls = scratch.join("calls");
            let spool = scratch.join("spool");
            fs::create_dir_all(&root).unwrap();
            fs::create_dir_all(&spool).unwrap();
            git(&root, &["init", "-q", "-b", "main"]);
            git(&root, &["init", "-q", "--bare", remote.to_str().unwrap()]);
            git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
            git(
                &root,
                &["remote", "add", "origin", remote.to_str().unwrap()],
            );
            git(
                &root,
                &[
                    "symbolic-ref",
                    "refs/remotes/origin/HEAD",
                    "refs/remotes/origin/main",
                ],
            );
            let upload = scratch.join("upload");
            fs::write(
                &upload,
                format!(
                    "#!/bin/sh\nprintf 'call\\n' >> '{}'\nexec git-upload-pack \"$@\"\n",
                    calls.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&upload, fs::Permissions::from_mode(0o700)).unwrap();
            git(
                &root,
                &[
                    "config",
                    "remote.origin.uploadpack",
                    upload.to_str().unwrap(),
                ],
            );
            let storage = Storage::open(&scratch.join("store")).unwrap();
            let f = Self {
                _scratch: scratch,
                root,
                remote,
                calls,
                upload,
                spool,
                storage,
            };
            f.commit("initial");
            f.push();
            f
        }

        fn commit(&self, text: &str) -> String {
            fs::write(self.root.join("source"), text).unwrap();
            git(&self.root, &["add", "source"]);
            git(
                &self.root,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "-q",
                    "-m",
                    text,
                ],
            );
            git(&self.root, &["rev-parse", "HEAD"])
        }

        fn push(&self) {
            git(&self.root, &["push", "-q", "origin", "main"]);
        }

        fn tip(&self) -> Option<String> {
            tip(&self.root, &self.storage, &self.spool)
        }

        fn tip_at(&self, now: Instant) -> Option<String> {
            tip_with_clock(
                &self.root,
                &self.storage,
                &self.spool,
                REMOTE_TIMEOUT,
                || now,
            )
        }

        fn calls(&self) -> usize {
            fs::read_to_string(&self.calls)
                .unwrap_or_default()
                .lines()
                .count()
        }
    }

    fn git(root: &Path, args: &[&str]) -> String {
        snapshot::git(root, args).unwrap()
    }

    #[test]
    fn local_tip_ahead_of_remote_has_no_tip() {
        let f = Fixture::new();
        let ahead = f.commit("unmerged");
        git(&f.root, &["update-ref", "refs/remotes/origin/main", &ahead]);
        assert_eq!(f.tip(), None);
        assert_eq!(f.calls(), 1);
    }

    #[test]
    fn equal_remote_default_branch_returns_tip() {
        let f = Fixture::new();
        assert_eq!(f.tip(), Some(git(&f.root, &["rev-parse", "HEAD"])));
        assert_eq!(f.calls(), 1);
    }

    #[test]
    fn unreachable_remote_has_no_tip() {
        let f = Fixture::new();
        fs::rename(&f.remote, f.remote.with_extension("offline")).unwrap();
        assert_eq!(f.tip(), None);
        assert_eq!(f.tip(), None);
        assert_eq!(f.calls(), 1);
    }

    #[test]
    fn cache_hit_avoids_second_ls_remote() {
        let f = Fixture::new();
        let expected = Some(git(&f.root, &["rev-parse", "HEAD"]));
        assert_eq!(f.tip(), expected);
        assert_eq!(f.tip(), expected);
        assert_eq!(f.calls(), 1);
    }

    #[test]
    fn concurrent_checks_share_one_successful_verification() {
        let f = Fixture::new();
        let expected = Some(git(&f.root, &["rev-parse", "HEAD"]));
        thread::scope(|scope| {
            let first = scope.spawn(|| f.tip());
            let second = scope.spawn(|| f.tip());
            assert_eq!(first.join().unwrap(), expected);
            assert_eq!(second.join().unwrap(), expected);
        });
        assert_eq!(f.calls(), 1);
    }

    #[test]
    fn changed_tip_reverifies() {
        let f = Fixture::new();
        assert!(f.tip().is_some());
        let next = f.commit("merged");
        f.push();
        assert_eq!(f.tip(), Some(next));
        assert_eq!(f.calls(), 2);
    }

    #[test]
    fn negative_cache_skips_second_query_and_warning_within_ttl() {
        let f = Fixture::new();
        f.commit("ahead of remote");
        let now = Instant::now();
        let captured = crate::logging::Captured::default();
        let _guard = crate::logging::scoped(captured.clone());
        assert_eq!(f.tip_at(now), None);
        assert_eq!(f.tip_at(now + Duration::from_secs(59)), None);
        assert_eq!(f.calls(), 1);
        assert_eq!(captured.text().lines().count(), 1);
    }

    #[test]
    fn negative_cache_rechecks_after_ttl() {
        let f = Fixture::new();
        f.commit("ahead of remote");
        let now = Instant::now();
        assert_eq!(f.tip_at(now), None);
        assert_eq!(f.tip_at(now + Duration::from_secs(30)), None);
        assert_eq!(f.tip_at(now + Duration::from_secs(60)), None);
        assert_eq!(f.calls(), 2);
    }

    #[test]
    fn negative_cache_changed_local_commit_rechecks_immediately() {
        let f = Fixture::new();
        f.commit("ahead of remote");
        let now = Instant::now();
        assert_eq!(f.tip_at(now), None);
        assert_eq!(f.tip_at(now + Duration::from_secs(1)), None);
        f.commit("another local commit");
        assert_eq!(f.tip_at(now + Duration::from_secs(2)), None);
        assert_eq!(f.calls(), 2);
    }

    #[test]
    fn changed_origin_url_reverifies() {
        let f = Fixture::new();
        assert!(f.tip().is_some());
        let other = f._scratch.join("other.git");
        git(
            &f.root,
            &[
                "clone",
                "--bare",
                "-q",
                f.remote.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        git(
            &f.root,
            &["remote", "set-url", "origin", other.to_str().unwrap()],
        );
        assert!(f.tip().is_some());
        assert_eq!(f.calls(), 2);
    }

    #[test]
    fn forged_origin_head_cannot_authorize_a_remote_feature_branch() {
        let f = Fixture::new();
        let branch = f.commit("unmerged feature");
        git(&f.root, &["branch", "feature", &branch]);
        git(&f.root, &["push", "-q", "origin", "feature"]);
        git(
            &f.root,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/feature",
            ],
        );
        assert_eq!(f.tip(), None);
        assert_eq!(f.calls(), 1);
    }

    #[test]
    fn mismatch_is_never_cached_as_success_and_recovers_after_ttl() {
        let f = Fixture::new();
        let next = f.commit("not yet pushed");
        let now = Instant::now();
        assert_eq!(f.tip_at(now), None);
        assert_eq!(f.tip_at(now + Duration::from_secs(1)), None);
        f.push();
        assert_eq!(f.tip_at(now + Duration::from_secs(2)), None);
        assert_eq!(f.tip_at(now + Duration::from_secs(60)), Some(next));
        assert_eq!(f.calls(), 2);
    }

    #[test]
    fn negative_cache_changed_origin_url_rechecks_immediately() {
        let f = Fixture::new();
        let next = f.commit("ahead of remote");
        let now = Instant::now();
        assert_eq!(f.tip_at(now), None);
        let other = f._scratch.join("other.git");
        git(
            &f.root,
            &[
                "clone",
                "--bare",
                "-q",
                f.root.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        git(&other, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(
            &f.root,
            &["remote", "set-url", "origin", other.to_str().unwrap()],
        );
        assert_eq!(f.tip_at(now + Duration::from_secs(1)), Some(next));
        assert_eq!(f.calls(), 2);
    }

    #[test]
    fn negative_cache_is_not_persisted() {
        let f = Fixture::new();
        assert!(f.tip().is_some());
        f.commit("ahead of remote");
        assert_eq!(f.tip(), None);
        assert_eq!(f.tip(), None);
        let store = f._scratch.join("store");
        let calls = f.calls.clone();
        let state = fs::read_to_string(store.join("state.json")).unwrap();
        assert!(!state.contains("failed_tips"));
        drop(f.storage);
        let storage = Storage::open(&store).unwrap();
        assert_eq!(tip(&f.root, &storage, &f.spool), None);
        assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 3);
    }

    #[test]
    fn successful_verification_survives_storage_reopen() {
        let f = Fixture::new();
        let expected = f.tip();
        assert!(expected.is_some());
        let store = f._scratch.join("store");
        let calls = f.calls.clone();
        drop(f.storage);
        let storage = Storage::open(&store).unwrap();
        assert_eq!(tip(&f.root, &storage, &f.spool), expected);
        assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    }

    #[test]
    fn child_is_noninteractive_and_preserves_home_and_path() {
        let f = Fixture::new();
        let environment = f._scratch.join("environment");
        fs::write(&f.upload, format!(
            "#!/bin/sh\nprintf '%s\\n' \"$GIT_TERMINAL_PROMPT\" \"$GCM_INTERACTIVE\" \"$GIT_SSH_COMMAND\" \"$GIT_ASKPASS\" \"$SSH_ASKPASS\" \"$SSH_ASKPASS_REQUIRE\" \"$HOME\" \"$PATH\" > '{}'\nexec git-upload-pack \"$@\"\n", environment.display())).unwrap();
        assert!(f.tip().is_some());
        let env = fs::read_to_string(environment).unwrap();
        let lines: Vec<_> = env.lines().collect();
        assert_eq!(
            &lines[..6],
            &[
                "0",
                "never",
                "ssh -o BatchMode=yes",
                "/usr/bin/false",
                "/usr/bin/false",
                "never"
            ]
        );
        assert_eq!(lines[6], std::env::var("HOME").unwrap());
        // Git may prepend its exec-path for helpers; the caller's PATH must remain intact.
        let path = std::env::var("PATH").unwrap();
        assert!(lines[7] == path || lines[7].ends_with(&format!(":{path}")));
    }

    #[test]
    fn auth_failure_is_negative_cached_and_logs_first_stderr_line_once() {
        let f = Fixture::new();
        fs::write(&f.upload, format!("#!/bin/sh\nprintf 'call\\n' >> '{}'\necho 'fatal: Authentication failed for origin' >&2\necho 'not the first line' >&2\nexit 1\n", f.calls.display())).unwrap();
        let captured = crate::logging::Captured::default();
        let _guard = crate::logging::scoped(captured.clone());
        assert_eq!(f.tip(), None);
        assert_eq!(f.tip(), None);
        let text = captured.text();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(f.calls(), 1);
        assert_eq!(lines.len(), 1, "{text}");
        for line in lines {
            assert!(line.contains("category=auth"), "{line}");
            assert!(
                line.contains("reason=fatal: Authentication failed for origin"),
                "{line}"
            );
            assert!(
                line.contains(&format!("repository={}", f.root.display())),
                "{line}"
            );
            assert!(line.contains("elapsed_ms="), "{line}");
            assert!(!line.contains("not the first line"), "{line}");
        }
    }

    #[test]
    fn hung_remote_times_out_and_kills_helpers() {
        let f = Fixture::new();
        let escaped = f._scratch.join("helper-survived");
        fs::write(
            &f.upload,
            format!(
                "#!/bin/sh\nprintf 'call\\n' >> '{}'\n(sleep 1; touch '{}') &\necho 'waiting for remote' >&2\nwait\n",
                f.calls.display(), escaped.display()
            ),
        )
        .unwrap();
        let captured = crate::logging::Captured::default();
        let _guard = crate::logging::scoped(captured.clone());
        let started = Instant::now();
        assert_eq!(
            tip_with_timeout(&f.root, &f.storage, &f.spool, Duration::from_millis(100)),
            None
        );
        assert_eq!(
            tip_with_timeout(&f.root, &f.storage, &f.spool, Duration::from_millis(100)),
            None
        );
        assert_eq!(f.calls(), 1);
        assert!(started.elapsed() < Duration::from_secs(2));
        let text = captured.text();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("category=timeout"), "{text}");
        assert!(text.contains("reason=waiting for remote"), "{text}");
        thread::sleep(Duration::from_millis(1200));
        assert!(
            !escaped.exists(),
            "timeout must kill the helper process group"
        );
    }
}
