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
