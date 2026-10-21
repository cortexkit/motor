//! A have reply tells the sender which commits it may leave out of its bundle.
//! Git applies a bundle whose prerequisite has no ref only when the history
//! behind that commit is still complete, and a mirror guarantees completeness
//! only for history a ref reaches. These tests keep a reported commit
//! referenced until its bundle arrives, even when the record that pinned it
//! goes away in between.
use super::*;

/// Explode a bare mirror's packs into loose objects, so a test can remove one
/// object the way an interrupted write or an expired cruft pack would.
fn loosen(mirror: &Path) {
    let packs = mirror.join("objects/pack");
    let aside = mirror.join("packs-aside");
    fs::create_dir_all(&aside).unwrap();
    for entry in fs::read_dir(&packs).unwrap().flatten() {
        fs::rename(entry.path(), aside.join(entry.file_name())).unwrap();
    }
    for entry in fs::read_dir(&aside).unwrap().flatten() {
        if entry.path().extension().is_some_and(|e| e == "pack") {
            let status = Command::new("git")
                .arg("-C")
                .arg(mirror)
                .args(["unpack-objects", "-q"])
                .stdin(fs::File::open(entry.path()).unwrap())
                .status()
                .unwrap();
            assert!(status.success());
        }
    }
    fs::remove_dir_all(aside).unwrap();
}

fn loose_object(mirror: &Path, oid: &str) -> PathBuf {
    mirror.join("objects").join(&oid[..2]).join(&oid[2..])
}

/// Submit the way a hosted client does: no integration tip, and the hello did
/// not opt in to default-branch tips, so no background cache fill is queued.
fn submit_hosted(f: &Fixture, runner: &Runner, request: &Run, bytes: &[Vec<u8>]) {
    assert_run_accepted(runner, request);
    let spool = f.spool(runner, request, bytes);
    runner.fetch_run(request, &spool.directory, false).unwrap();
}

/// A thin bundle of the main repository's HEAD with `held` as prerequisite.
fn thin_bundle(f: &Fixture, held: &str) -> (BundleManifest, Vec<u8>) {
    let id = Uuid::now_v7();
    let reference = format!("refs/ck-motor/{id}/snapshot");
    git(&f.main, &["update-ref", &reference, "HEAD"]);
    let path = f.scratch.join(format!("{id}.bundle"));
    git(
        &f.main,
        &[
            "bundle",
            "create",
            text(&path),
            &reference,
            &format!("^{held}"),
        ],
    );
    git(&f.main, &["update-ref", "-d", &reference]);
    let heads = git(&f.main, &["bundle", "list-heads", text(&path)]);
    assert!(heads.contains(&reference));
    let bytes = fs::read(path).unwrap();
    // The bundle header names its prerequisites as `-<commit>` lines.
    assert!(String::from_utf8_lossy(&bytes).contains(&format!("\n-{held}")));
    (
        BundleManifest {
            index: 0,
            repository: f.root.clone(),
            length: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        },
        bytes,
    )
}

/// The runner reports a commit while a job record still pins it. The record
/// then goes away and a prune runs before the bundle arrives, as when a job's
/// retention or a workspace ends during a slow upload or a tenant's admission
/// wait. The mirror no longer has the reported commit's tree. Git would accept
/// that only while a ref reaches the commit, so the reply must keep one.
#[tokio::test]
async fn reported_commit_stays_a_valid_prerequisite_after_its_record_is_pruned() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut first, bytes) = job_at(&f, "first", "true");
    first.integration_tip = String::new();
    submit_hosted(&f, &runner, &first, &bytes);
    assert_eq!(
        terminal(&runner, first.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let held_commit = first.snapshot_commit.clone();
    let mirror = runner.core.trees.mirror(&f.root);

    let (next, _) = commit(&f.main, "second\n");
    let held = runner.have(vec![RepositoryCandidates {
        repository: f.root.clone(),
        commits: vec![next.clone(), held_commit.clone()],
    }]);
    assert_eq!(held[0].commits, vec![held_commit.clone()]);

    // The only records that pinned the commit end before the bundle arrives.
    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.jobs.remove(&first.job_id);
        inner.catalog.workspaces.remove(&first.workspace_key);
    }
    runner.prune_refs().unwrap();
    let tree = git(&mirror, &["rev-parse", &format!("{held_commit}^{{tree}}")]);
    loosen(&mirror);
    fs::remove_file(loose_object(&mirror, &tree)).unwrap();

    let (mut second, mut bytes) = job_at(&f, "second", "true");
    second.integration_tip = String::new();
    let (manifest, thin) = thin_bundle(&f, &held_commit);
    second.bundles[0] = manifest;
    bytes[0] = thin;
    submit_hosted(&f, &runner, &second, &bytes);
    let result = terminal(&runner, second.job_id).await;
    assert_eq!(
        result.outcome,
        Outcome::Exit { code: 0 },
        "bundle built from the have reply was rejected: {:?}",
        result.refusal_detail()
    );
    assert!(
        !loose_object(&mirror, &tree).exists()
            && workspace::git(&mirror, &["cat-file", "-e", &tree]).is_err(),
        "the reported commit's tree must still be missing for this to prove anything"
    );
}

/// Held refs are released once their offer expires, and a commit no ref
/// reaches any more is not reported, so the sender sends it again.
#[tokio::test]
async fn held_refs_expire_and_unreferenced_commits_are_not_reported() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (mut first, bytes) = f.run("true");
    first.integration_tip = String::new();
    submit_hosted(&f, &runner, &first, &bytes);
    assert_eq!(
        terminal(&runner, first.job_id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    let mirror = runner.core.trees.mirror(&f.root);
    let have = || {
        runner.have(vec![RepositoryCandidates {
            repository: f.root.clone(),
            commits: vec![first.snapshot_commit.clone()],
        }])
    };
    assert_eq!(have()[0].commits, vec![first.snapshot_commit.clone()]);
    let held = || {
        git(
            &mirror,
            &["for-each-ref", "--format=%(refname)", "refs/held"],
        )
    };
    assert_eq!(held(), format!("refs/held/{}", first.snapshot_commit));

    {
        let mut inner = runner.core.inner.lock().unwrap();
        inner.jobs.remove(&first.job_id);
        inner.catalog.workspaces.remove(&first.workspace_key);
    }
    runner.prune_refs().unwrap();
    // The offer is still live: its ref survives the prune but is not itself
    // a reason to report the commit again.
    assert_eq!(held(), format!("refs/held/{}", first.snapshot_commit));
    assert!(git(&mirror, &["for-each-ref", "refs/snapshots"]).is_empty());
    f.clock.fetch_add(HELD_OFFER_MS + 1, Ordering::SeqCst);
    runner.prune_refs().unwrap();
    assert!(held().is_empty(), "expired held refs remain: {}", held());

    // The objects are still in the mirror, but no ref reaches them.
    assert!(workspace::git(&mirror, &["cat-file", "-e", &first.snapshot_commit]).is_ok());
    assert!(have()[0].commits.is_empty());
}

/// The bundle a sender builds from a have reply, as `Snapshots::bundle` does:
/// thin against a reported commit, otherwise the whole history. When the
/// runner already holds the snapshot commit (and any tip), `Snapshots::bundle`
/// sends no bundle; these fixtures need one bundle per repository, so the
/// whole history stands in for that case.
fn bundle_for(f: &Fixture, held: &[RepositoryCandidates]) -> (BundleManifest, Vec<u8>) {
    let head = git(&f.main, &["rev-parse", "HEAD"]);
    match held[0].commits.iter().find(|commit| **commit != head) {
        Some(commit) => thin_bundle(f, commit),
        None => f.bundle(&f.main, &f.root, 0),
    }
}

/// One hosted-style job on its own workspace whose main bundle follows a
/// fresh have reply, run to its terminal record.
async fn job_after_have(f: &Fixture, runner: &Runner, name: &str) -> TerminalRecord {
    let held = runner.have(vec![RepositoryCandidates {
        repository: f.root.clone(),
        commits: vec![git(&f.main, &["rev-parse", "HEAD"])],
    }]);
    let (mut run, mut bytes) = job_at(f, name, "true");
    run.integration_tip = String::new();
    let (manifest, data) = bundle_for(f, &held);
    run.bundles[0] = manifest;
    bytes[0] = data;
    submit_hosted(f, runner, &run, &bytes);
    terminal(runner, run.job_id).await
}

/// Damage an unclean VM stop can leave in a mirror: a ref file that is empty
/// because its write never reached the disk, or a ref at a commit object that
/// never did. Git's connectivity check reads every ref, so either one fails
/// every later fetch into the mirror. The empty file is the production
/// failure: the have reply still reports the intact refs, and the thin bundle
/// is refused as "not connected to the repository's history". The first job
/// after the damage must be refused as an interrupted transfer that says the
/// mirror was repaired, and the next job's bundle must apply.
async fn repaired_after_broken_ref(damage: &str) {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let first = job_after_have(&f, &runner, "first").await;
    assert_eq!(first.outcome, Outcome::Exit { code: 0 }, "{damage}");
    let mirror = runner.core.trees.mirror(&f.root);
    let lost = "1".repeat(40);
    let content = if damage == "empty ref file" {
        String::new()
    } else {
        format!("{lost}\n")
    };
    // A record still pins the damaged ref, as the last job's workspace record
    // pins that job's snapshot, so pruning keeps it. Damage the mirror under
    // the ref lock, so a scheduler prune sees the record and the ref together.
    {
        let _refs = runner.core.ref_mutation.lock().unwrap();
        runner
            .core
            .inner
            .lock()
            .unwrap()
            .catalog
            .workspaces
            .values_mut()
            .for_each(|workspace| workspace.snapshot = lost.clone());
        fs::write(mirror.join("refs/snapshots").join(&lost), content).unwrap();
    }

    commit(&f.main, "second\n");
    let refused = job_after_have(&f, &runner, "second").await;
    let retried = job_after_have(&f, &runner, "third").await;
    assert_eq!(
        (&refused.outcome, &retried.outcome),
        (
            &Outcome::RefusedBeforeStart {
                reason: RefusalReason::TransferInterrupted
            },
            &Outcome::Exit { code: 0 }
        ),
        "{damage}: {:?} then {:?}",
        refused.refusal_detail(),
        retried.refusal_detail()
    );
    let detail = refused.refusal_detail().unwrap();
    let reason = if damage == "empty ref file" {
        "unreadable ref file"
    } else {
        "commit missing"
    };
    assert!(
        detail.starts_with("mirror_repaired:")
            && detail.contains(&format!("refs/snapshots/{lost} ({reason})")),
        "{damage}: {detail}"
    );
    assert!(!mirror.join("refs/snapshots").join(&lost).exists());
}

#[tokio::test]
async fn an_empty_mirror_ref_file_is_removed_after_the_fetch_it_breaks() {
    repaired_after_broken_ref("empty ref file").await;
}

#[tokio::test]
async fn a_mirror_ref_at_a_missing_commit_is_removed_after_the_fetch_it_breaks() {
    repaired_after_broken_ref("missing commit").await;
}

/// A runner that restarts after an unclean stop removes refs it cannot use
/// before serving: an empty ref file, a ref at a missing commit, and a ref at
/// a commit whose tree is missing. Intact refs stay.
#[tokio::test]
async fn startup_removes_unusable_mirror_refs() {
    let f = Fixture::new();
    let config = f.config();
    let runner = Runner::open(config.clone()).unwrap();
    let first = job_after_have(&f, &runner, "first").await;
    assert_eq!(first.outcome, Outcome::Exit { code: 0 });
    let mirror = runner.core.trees.mirror(&f.root);
    drop(runner);

    let snapshot = git(&f.main, &["rev-parse", "HEAD"]);
    let refs = mirror.join("refs/snapshots");
    fs::write(refs.join("empty"), "").unwrap();
    fs::write(refs.join("lost"), format!("{}\n", "2".repeat(40))).unwrap();
    // A commit object that made it to disk without its tree.
    let (orphan, tree) = commit(&f.main, "orphan\n");
    let packed = Command::new("git")
        .arg("-C")
        .arg(&f.main)
        .args(["pack-objects", "--stdout", "-q"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = packed.stdin.as_ref().unwrap();
        writeln!(stdin, "{orphan}").unwrap();
    }
    let pack = packed.wait_with_output().unwrap().stdout;
    let mut unpack = Command::new("git")
        .arg("-C")
        .arg(&mirror)
        .args(["unpack-objects", "-q"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        unpack.stdin.take().unwrap().write_all(&pack).unwrap();
    }
    assert!(unpack.wait().unwrap().success());
    assert!(workspace::git(&mirror, &["cat-file", "-e", &tree]).is_err());
    fs::write(refs.join("treeless"), format!("{orphan}\n")).unwrap();

    let runner = reopen(config);
    // for-each-ref skips an empty ref file, so check the files themselves.
    for name in ["empty", "lost", "treeless"] {
        assert!(
            !refs.join(name).exists(),
            "startup kept refs/snapshots/{name}"
        );
    }
    let remaining = git(
        &mirror,
        &["for-each-ref", "--format=%(refname)", "refs/snapshots"],
    );
    assert_eq!(remaining, format!("refs/snapshots/{snapshot}"));
    drop(runner);
}
