use super::recovery::{Cut, Fixture};
use crate::snapshot::{self, Snapshots};
use cortexkit_test_support::ScratchDir;
use motor_protocol::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

fn git(path: &Path, args: &[&str]) -> String {
    snapshot::git(path, args).unwrap()
}
fn lock(sources: &[String]) -> String {
    sources
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!("[[package]]\nname = \"p{i}\"\nversion = \"0.1.0\"\nsource = {s:?}\n")
        })
        .collect()
}
fn bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                result.insert(path.strip_prefix(root).unwrap().to_owned(), vec![]);
                visit(root, &path, result);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}
#[test]
fn lockfile_parses_queries_escapes_duplicates_and_multiple_commits() {
    let a = "a".repeat(40);
    let b = "b".repeat(64);
    let url = "https://example.invalid/repo%20name.git";
    let mut sources: Vec<_> = ["", "?branch=feature%2Fone", "?tag=v1", "?rev=abc"]
        .into_iter()
        .map(|query| format!("git+{url}{query}#{a}"))
        .collect();
    sources.extend([
        format!("git+{url}#{b}"),
        "registry+https://registry.invalid".into(),
        format!("git+{url}#BAD"),
        format!("git+{url}"),
    ]);
    assert_eq!(
        snapshot::parse_lockfile(&lock(&sources)).unwrap(),
        BTreeMap::from([(url.into(), vec![a, b])])
    );
}
#[tokio::test]
async fn run_lockfile_source_is_commit_not_worktree() {
    let f = Fixture::new(Cut::None, 28).await;
    let url = "https://example.invalid/locked.git";
    let a = "a".repeat(40);
    let b = "b".repeat(40);
    fs::write(f.key.join("Cargo.lock"), lock(&[format!("git+{url}#{a}")])).unwrap();
    git(&f.key, &["add", "Cargo.lock"]);
    git(
        &f.key,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "locked",
        ],
    );
    fs::write(f.key.join("Cargo.lock"), lock(&[format!("git+{url}#{a}")])).unwrap();
    let s = Snapshots::build(
        &f.config,
        f.key.to_str().unwrap(),
        f.main.to_str().unwrap(),
        &[],
        &f.scratch.join("spool"),
        Uuid::now_v7(),
        None,
        &crate::storage::Storage::open(&f.scratch.join("capture-store")).unwrap(),
    )
    .unwrap_or_else(|_| panic!("snapshot failed"));
    fs::write(f.key.join("Cargo.lock"), lock(&[format!("git+{url}#{b}")])).unwrap();
    assert_eq!(
        snapshot::lockfile_sources(&s.main.path, &s.main.commit).unwrap(),
        BTreeMap::from([(url.into(), vec![a.clone()])])
    );
    assert_eq!(
        s.git_dependencies,
        BTreeMap::from([(url.into(), vec![a.clone()])])
    );
    assert!(
        snapshot::lockfile_sources(&f.main, &git(&f.main, &["rev-parse", "HEAD"]))
            .unwrap()
            .is_empty()
    );
}
fn isolated_tip_cargo_home(test: &str) -> bool {
    const CHILD: &str = "MOTOR_TEST_TIP_DEPENDENCIES_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return false;
    }
    // Cargo-home selection is process-wide; isolate it from parallel tests.
    let home = ScratchDir::new("motor-tip-cargo-home");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("tests::git_dependencies::{test}"),
            "--nocapture",
        ])
        .env(CHILD, test)
        .env("CARGO_HOME", home.as_ref())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "tip dependency job failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn commit_all(path: &Path, message: &str) -> String {
    git(path, &["add", "."]);
    git(
        path,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            message,
        ],
    );
    git(path, &["rev-parse", "HEAD"])
}

fn tip_cargo_home() -> PathBuf {
    PathBuf::from(std::env::var_os("CARGO_HOME").unwrap())
}

const TIP_URL: &str = "https://example.invalid/commons.git";

async fn tip_dependency_fixture(cache_tip: bool) -> (Fixture, String, String) {
    let f = Fixture::new(Cut::None, 28).await;
    let dependency = f.scratch.join("dependency");
    fs::create_dir(&dependency).unwrap();
    git(&dependency, &["init"]);
    fs::write(dependency.join("source"), "revision A\n").unwrap();
    let a = commit_all(&dependency, "dependency A");
    let cache = tip_cargo_home().join("git/db/commons-test");
    fs::create_dir_all(&cache).unwrap();
    git(&cache, &["init", "--bare"]);
    git(
        &cache,
        &[
            "fetch",
            dependency.to_str().unwrap(),
            &format!("{a}:refs/heads/cached"),
        ],
    );
    fs::write(dependency.join("source"), "revision B\n").unwrap();
    let b = commit_all(&dependency, "dependency B");
    if cache_tip {
        git(
            &cache,
            &[
                "fetch",
                dependency.to_str().unwrap(),
                &format!("{b}:refs/heads/cached"),
            ],
        );
    }
    git(&f.main, &["branch", "-M", "main"]);
    fs::write(
        f.main.join("Cargo.lock"),
        lock(&[format!("git+{TIP_URL}#{b}")]),
    )
    .unwrap();
    commit_all(&f.main, "default branch locks B");
    let origin = f.scratch.join("origin.git");
    git(&f.main, &["clone", "--bare", ".", origin.to_str().unwrap()]);
    git(
        &f.main,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    fs::write(
        f.key.join("Cargo.lock"),
        lock(&[format!("git+{TIP_URL}#{a}")]),
    )
    .unwrap();
    commit_all(&f.key, "worktree locks A");
    (f, a, b)
}

#[tokio::test]
async fn cached_tip_dependency_is_offered_without_changing_snapshot() {
    if isolated_tip_cargo_home("cached_tip_dependency_is_offered_without_changing_snapshot") {
        return;
    }
    use super::recovery::{collect, terminal};
    let (f, a, b) = tip_dependency_fixture(true).await;
    let snapshot = git(&f.key, &["rev-parse", "HEAD"]);
    let tree = git(&f.key, &["rev-parse", "HEAD^{tree}"]);
    let tip = git(&f.main, &["rev-parse", "HEAD"]);
    let cache_before = bytes(&tip_cargo_home());
    let motor = f.motor();
    for cold in [true, false] {
        f.proxy.frames.lock().unwrap().clear();
        let records = collect(motor.run(f.request("true")).unwrap()).await;
        assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
        let frames = f.proxy.frames.lock().unwrap();
        let run = frames
            .iter()
            .find_map(|frame| match frame {
                Frame::Run(run) => Some(run),
                _ => None,
            })
            .unwrap();
        assert_eq!(run.snapshot_commit, snapshot);
        assert_eq!(run.tree_hash, tree);
        assert_eq!(run.integration_tip, tip);
        assert_eq!(run.git_dependencies.len(), 2, "tip-only B was not offered");
        for commit in [&a, &b] {
            let dependency = run
                .git_dependencies
                .iter()
                .find(|dependency| {
                    dependency.url == TIP_URL && dependency.commits == [commit.clone()]
                })
                .expect("locked commit offered");
            assert_eq!(dependency.bundle.is_some(), cold);
            if let Some(bundle) = &dependency.bundle {
                assert_eq!(bundle.repository, TIP_URL);
                assert!(bundle.length > 0);
                assert!(frames.iter().any(|frame| matches!(frame, Frame::BundleChunk { index, .. } if *index == bundle.index)));
            }
        }
        if cold {
            assert_eq!(bytes(&tip_cargo_home()), cache_before);
            // The second job must rely on Have, not on the client's cache.
            fs::remove_dir_all(tip_cargo_home().join("git/db/commons-test")).unwrap();
        }
    }
    assert!(snapshot::cached_commit(&tip_cargo_home(), TIP_URL, &b).is_none());
    assert_eq!(git(&f.key, &["rev-parse", "HEAD"]), snapshot);
    assert_eq!(
        fs::read_to_string(f.key.join("Cargo.lock")).unwrap(),
        lock(&[format!("git+{TIP_URL}#{a}")])
    );
}

#[tokio::test]
async fn missing_tip_only_dependency_does_not_block_job() {
    if isolated_tip_cargo_home("missing_tip_only_dependency_does_not_block_job") {
        return;
    }
    use super::recovery::{collect, terminal};
    let (f, a, b) = tip_dependency_fixture(false).await;
    // A new URL at the tip must participate in Have even when it cannot be
    // supplied. The required snapshot URL still carries only its cached A.
    let extra_url = "https://example.invalid/new-tip-dependency.git";
    let unsupported = "file:///unavailable-tip-dependency";
    fs::write(
        f.main.join("Cargo.lock"),
        lock(&[
            format!("git+{TIP_URL}#{b}"),
            format!("git+{extra_url}#{b}"),
            format!("git+{unsupported}#{b}"),
        ]),
    )
    .unwrap();
    commit_all(&f.main, "default branch adds dependency URL");
    git(
        &f.scratch.join("origin.git"),
        &[
            "fetch",
            f.main.to_str().unwrap(),
            "refs/heads/main:refs/heads/main",
        ],
    );
    let snapshots = Snapshots::build(
        &f.config,
        f.key.to_str().unwrap(),
        f.main.to_str().unwrap(),
        &[],
        &f.scratch.join("spool"),
        Uuid::now_v7(),
        None,
        &crate::storage::Storage::open(&f.scratch.join("capture-store")).unwrap(),
    )
    .unwrap_or_else(|_| panic!("snapshot failed"));
    assert!(snapshots.git_dependencies[TIP_URL].contains(&b));
    assert!(snapshots.git_dependencies.contains_key(extra_url));
    assert!(!snapshots.git_dependencies.contains_key(unsupported));
    assert!(snapshot::cached_commit(&tip_cargo_home(), TIP_URL, &b).is_none());
    let motor = f.motor();
    let records = collect(motor.run(f.request("true")).unwrap()).await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    let frames = f.proxy.frames.lock().unwrap();
    assert!(frames.iter().any(|frame| matches!(frame, Frame::Have { git_dependencies, .. } if git_dependencies.contains(&TIP_URL.into()) && git_dependencies.contains(&extra_url.into()))));
    let run = frames
        .iter()
        .find_map(|frame| match frame {
            Frame::Run(run) => Some(run),
            _ => None,
        })
        .unwrap();
    assert_eq!(run.git_dependencies.len(), 1);
    assert_eq!(run.git_dependencies[0].url, TIP_URL);
    assert_eq!(run.git_dependencies[0].commits, vec![a]);
    assert!(run.git_dependencies[0].bundle.is_some());
}

#[test]
fn cache_lookup_bundle_is_complete_single_ref_and_cache_byte_identical() {
    let scratch = ScratchDir::new("motor-dependency-cache");
    let source = scratch.join("source");
    fs::create_dir_all(&source).unwrap();
    git(&source, &["init"]);
    fs::write(source.join("lib.rs"), "pub fn x() {}\n").unwrap();
    git(&source, &["add", "."]);
    git(
        &source,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "source",
        ],
    );
    let commit = git(&source, &["rev-parse", "HEAD"]);
    let home = scratch.join("cargo");
    let db = home.join("git/db");
    fs::create_dir_all(&db).unwrap();
    let wrong = db.join("private-aaa");
    fs::create_dir(&wrong).unwrap();
    git(&wrong, &["init", "--bare"]);
    let cache = db.join("private-zzz");
    git(
        &source,
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            cache.to_str().unwrap(),
        ],
    );
    let url = "https://unroutable.invalid/private.git";
    assert_eq!(
        snapshot::cached_commit(&home, url, &commit),
        Some(cache.clone())
    );
    let sources = BTreeMap::from([(url.into(), vec![commit.clone(), "a".repeat(40)])]);
    let before = bytes(&home);
    let spool = scratch.join("spool");
    fs::create_dir(&spool).unwrap();
    let mut upload = vec![];
    let deps = snapshot::dependency_bundles(
        &sources,
        &BTreeSet::new(),
        Some(&[]),
        &home,
        &spool,
        &mut upload,
        Uuid::now_v7(),
    )
    .unwrap();
    assert_eq!(bytes(&home), before);
    assert_eq!(deps.len(), 2);
    assert!(deps[1].bundle.is_none());
    assert_eq!(upload.len(), 1);
    let bundle = spool.join("bundle-0");
    assert_eq!(
        git(&source, &["bundle", "list-heads", bundle.to_str().unwrap()]),
        format!("{commit} refs/ck-motor/dep/{commit}")
    );
    let empty = scratch.join("empty");
    fs::create_dir(&empty).unwrap();
    git(&empty, &["init", "--bare"]);
    git(&empty, &["bundle", "verify", bundle.to_str().unwrap()]);
    git(
        &empty,
        &[
            "fetch",
            bundle.to_str().unwrap(),
            &format!("refs/ck-motor/dep/{commit}:refs/ck-motor/dep/{commit}"),
        ],
    );
    assert_eq!(git(&empty, &["cat-file", "-t", &commit]), "commit");
    let mut upload = vec![];
    let held = [GitDependencyCandidates {
        url: url.into(),
        commits: vec![commit],
    }];
    assert!(snapshot::dependency_bundles(
        &sources,
        &BTreeSet::new(),
        Some(&held),
        &home,
        &spool,
        &mut upload,
        Uuid::now_v7()
    )
    .unwrap()
    .iter()
    .all(|d| d.bundle.is_none()));
    assert!(upload.is_empty());
}

#[test]
fn old_runner_without_capability_receives_no_dependency_chunks() {
    let scratch = ScratchDir::new("motor-old-runner-dependencies");
    let spool = scratch.join("spool");
    fs::create_dir(&spool).unwrap();
    let home = scratch.join("cargo");
    let cache = home.join("git/db/private-fakehash");
    fs::create_dir_all(&cache).unwrap();
    git(&cache, &["init"]);
    fs::write(cache.join("source"), "dependency").unwrap();
    git(&cache, &["add", "."]);
    git(
        &cache,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "cache",
        ],
    );
    let commit = git(&cache, &["rev-parse", "HEAD"]);
    let sources = BTreeMap::from([("https://example.invalid/private.git".into(), vec![commit])]);
    let mut upload = vec![];
    assert!(snapshot::dependency_bundles(
        &sources,
        &BTreeSet::new(),
        None,
        &home,
        &spool,
        &mut upload,
        Uuid::now_v7()
    )
    .unwrap()
    .is_empty());
    assert!(upload.is_empty());
    assert_eq!(
        snapshot::dependency_bundles(
            &sources,
            &BTreeSet::new(),
            Some(&[]),
            &home,
            &spool,
            &mut upload,
            Uuid::now_v7()
        )
        .unwrap()
        .len(),
        1
    );
    assert_eq!(upload.len(), 1);
}

#[test]
fn dependency_submodules_warning_names_url() {
    let scratch = ScratchDir::new("motor-dependency-submodules");
    git(&scratch, &["init"]);
    fs::write(
        scratch.join(".gitmodules"),
        "[submodule \"private\"]\npath = sub\nurl = https://example.invalid/sub.git\n",
    )
    .unwrap();
    git(&scratch, &["add", "."]);
    git(
        &scratch,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "submodules",
        ],
    );
    let commit = git(&scratch, &["rev-parse", "HEAD"]);
    let url = "https://example.invalid/private.git";
    let warning = snapshot::submodule_warning(&scratch, url, &commit).unwrap();
    assert!(warning.contains(url));
    assert!(warning.contains("submodule repositories are not shipped"));
}

#[tokio::test]
async fn legacy_runner_capability_suppresses_run_dependency_frames() {
    use super::recovery::{collect, terminal};
    use std::sync::atomic::Ordering;
    let f = Fixture::new(Cut::None, 28).await;
    f.proxy.legacy_have.store(true, Ordering::SeqCst);
    fs::write(
        f.key.join("Cargo.lock"),
        lock(&[format!(
            "git+https://example.invalid/private.git#{}",
            "a".repeat(40)
        )]),
    )
    .unwrap();
    git(&f.key, &["add", "Cargo.lock"]);
    git(
        &f.key,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "locked",
        ],
    );
    let motor = f.motor();
    let records = collect(motor.run(f.request("true")).unwrap()).await;
    assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
    let frames = f.proxy.frames.lock().unwrap();
    assert!(frames.iter().any(|frame| matches!(frame, Frame::Have { git_dependencies, .. } if git_dependencies == &["https://example.invalid/private.git"])));
    let manifests = frames
        .iter()
        .find_map(|frame| match frame {
            Frame::Run(r) => {
                assert!(r.git_dependencies.is_empty());
                Some(&r.bundles)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(manifests.len(), 2);
    assert!(frames
        .iter()
        .any(|frame| matches!(frame, Frame::BundleChunk { index: 1, .. })));
    assert!(!frames
        .iter()
        .any(|frame| matches!(frame, Frame::BundleChunk { index, .. } if *index >= 2)));
}
