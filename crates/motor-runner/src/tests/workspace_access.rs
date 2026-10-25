//! A job owns its workspace root while it runs and can lock the runner
//! account out of it. The next admission on that workspace must not be
//! refused forever: the runner repairs its access and admits the job, and a
//! workspace it cannot repair is refused with a detail that names the
//! runner's copy, the path, the step, the errno and the operator remedy.
//!
//! A portable fixture runs jobs as the runner's own account, which still
//! reads a root it owns at mode 0700, so these tests lock the root at mode
//! 0000 instead. The real chmod 0700 on a root that carries the runner's ACL
//! entry, done by the job account, is
//! `isolated_targets::isolated_job_that_locks_its_workspace_root_does_not_wedge_later_admissions`.
use super::*;
use std::os::unix::fs::PermissionsExt;

/// Puts the workspace root's mode back when the test ends, so a failed
/// assertion still leaves a scratch tree the fixture can delete.
struct Unlock(PathBuf);

impl Drop for Unlock {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

/// Act as a job that locked the runner out of its workspace root.
fn lock_out(root: &str) -> Unlock {
    fs::set_permissions(root, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        fs::read_dir(root).is_err(),
        "the fixture must run as an account that a mode-0000 directory locks out, not root"
    );
    Unlock(PathBuf::from(root))
}

fn refusal_detail(runner: &Runner, id: Uuid) -> String {
    runner.core.inner.lock().unwrap().jobs[&id]
        .terminal
        .as_ref()
        .and_then(|terminal| terminal.refusal_detail().map(str::to_owned))
        .expect("refused job carries a refusal detail")
}

#[tokio::test]
async fn admission_repairs_a_workspace_a_job_locked_and_runs_the_next_job() {
    let f = Fixture::new();
    let repaired = Arc::new(Mutex::new(Vec::new()));
    let mut config = f.config();
    let seen = repaired.clone();
    config.workspace_access_repair = Some(Arc::new(move |root: &Path| {
        seen.lock().unwrap().push(root.to_path_buf());
        fs::set_permissions(root, fs::Permissions::from_mode(0o755))
    }));
    let runner = Runner::open(config).unwrap();
    let (first, bytes) = f.run("true");
    f.submit(&runner, &first, &bytes);
    assert_eq!(
        terminal(&runner, first.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let _unlock = lock_out(&f.key);

    let (second, bytes) = f.run("test -r source");
    f.submit(&runner, &second, &bytes);
    let ended = terminal(&runner, second.job_id).await;
    assert_eq!(
        ended.outcome,
        Outcome::Exit { code: 0 },
        "refusal detail: {:?}",
        ended.refusal_detail()
    );
    assert_eq!(*repaired.lock().unwrap(), [PathBuf::from(&f.key)]);
}

#[test]
fn a_workspace_the_runner_cannot_repair_is_refused_with_the_side_the_step_and_the_remedy() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    fs::create_dir_all(&f.key).unwrap();
    let _unlock = lock_out(&f.key);
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request.clone()).unwrap(),
        Admission::Refused(RefusalReason::WorkspaceKeyRejected)
    ));
    let detail = refusal_detail(&runner, request.job_id);
    for expected in [
        "the runner's copy of the workspace on the runner's host is unreadable to the runner account",
        &format!("path={}", f.key),
        "step=listing the directory for runner base checkouts",
        "errno=EACCES (Permission denied (os error 13))",
        "the runner did not repair it: this runner does not isolate jobs",
        "retrying will not help until an operator repairs it on the runner's host",
        &format!(
            "sudo setfacl -R -P -m u:{}:rwX -- '{}'",
            unsafe { libc::geteuid() },
            f.key
        ),
    ] {
        assert!(detail.contains(expected), "missing {expected:?} in {detail}");
    }
}

#[test]
fn a_failed_or_ineffective_repair_is_named_in_the_refusal() {
    for (helper, expected) in [
        ("fails", "the runner could not restore its access to"),
        ("does nothing", "the runner restored its access to"),
    ] {
        let f = Fixture::new();
        let calls = Arc::new(AtomicU64::new(0));
        let mut config = f.config();
        let counted = calls.clone();
        config.workspace_access_repair = Some(Arc::new(move |_: &Path| {
            counted.fetch_add(1, Ordering::SeqCst);
            if helper == "fails" {
                Err(io::Error::other("simulated helper failure"))
            } else {
                Ok(())
            }
        }));
        let runner = Runner::open(config).unwrap();
        fs::create_dir_all(&f.key).unwrap();
        let _unlock = lock_out(&f.key);
        let (request, _) = f.run("true");
        assert!(matches!(
            runner.accept(request.clone()).unwrap(),
            Admission::Refused(RefusalReason::WorkspaceKeyRejected)
        ));
        let detail = refusal_detail(&runner, request.job_id);
        assert!(
            detail.contains(&format!("{expected} {}", f.key)),
            "helper {helper}: {detail}"
        );
        assert!(
            detail.contains("an operator repairs it"),
            "helper {helper}: {detail}"
        );
        if helper == "fails" {
            assert!(detail.contains("simulated helper failure"), "{detail}");
        } else {
            assert!(
                detail.contains("but the path is still unreadable"),
                "{detail}"
            );
        }
        // One repair per checkout and admission: a repair that did not hold
        // is not retried in a loop.
        assert_eq!(calls.load(Ordering::SeqCst), 1, "helper {helper}");
    }
}

/// A job admitted while the workspace was readable reaches setup after the
/// previous job locked the runner out; setup's own path checks, which run
/// before its reclaim, repair the root the same way admission does.
#[tokio::test]
async fn setup_repairs_a_workspace_locked_after_admission() {
    let f = Fixture::new();
    let repaired = Arc::new(AtomicU64::new(0));
    let mut config = f.config();
    let counted = repaired.clone();
    config.workspace_access_repair = Some(Arc::new(move |root: &Path| {
        counted.fetch_add(1, Ordering::SeqCst);
        fs::set_permissions(root, fs::Permissions::from_mode(0o755))
    }));
    let runner = Runner::open(config).unwrap();
    let (first, bytes) = f.run("true");
    f.submit(&runner, &first, &bytes);
    assert_eq!(
        terminal(&runner, first.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let (second, bytes) = f.run("test -r source");
    assert_run_accepted(&runner, &second);
    let _unlock = lock_out(&f.key);
    let spool = f.spool(&runner, &second, &bytes);
    runner.fetch_run(&second, &spool.directory, true).unwrap();
    let ended = terminal(&runner, second.job_id).await;
    assert_eq!(
        ended.outcome,
        Outcome::Exit { code: 0 },
        "refusal detail: {:?}",
        ended.refusal_detail()
    );
    assert!(repaired.load(Ordering::SeqCst) >= 1);
}
