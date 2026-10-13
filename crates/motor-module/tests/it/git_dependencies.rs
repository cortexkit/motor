use super::recovery::{Cut, Fixture};
use crate::snapshot::{self, Snapshots};
use cortexkit_test_support::ScratchDir;
use motor_protocol::*;
use std::{
    collections::BTreeMap,
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
async fn run_and_prepare_lockfile_source_is_commit_not_worktree() {
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
    let base = git(&f.key, &["rev-parse", "HEAD"]);
    for prepare in [false, true] {
        fs::write(
            f.key.join("Cargo.lock"),
            lock(&[format!("git+{url}#{}", if prepare { &b } else { &a })]),
        )
        .unwrap();
        let s = Snapshots::build(
            &f.config,
            f.key.to_str().unwrap(),
            f.main.to_str().unwrap(),
            &[],
            &f.scratch.join("spool"),
            Uuid::now_v7(),
            prepare.then_some(base.as_str()),
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
    }
    assert!(
        snapshot::lockfile_sources(&f.main, &git(&f.main, &["rev-parse", "HEAD"]))
            .unwrap()
            .is_empty()
    );
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
async fn legacy_runner_capability_suppresses_run_and_prepare_dependency_frames() {
    use super::recovery::{collect, terminal};
    use std::sync::atomic::Ordering;
    for preparing in [false, true] {
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
        if preparing {
            let request: PrepareRequest = serde_json::from_value(serde_json::json!({"workspace_key": f.key, "repository_root": f.main, "base_commit": git(&f.key, &["rev-parse", "HEAD"]), "siblings": [f.sibling]})).unwrap();
            assert_eq!(
                motor.prepare(request).await.unwrap().outcome,
                PrepareOutcome::Prepared
            );
        } else {
            let records = collect(motor.run(f.request("true")).unwrap()).await;
            assert_eq!(terminal(&records).outcome, Outcome::Exit { code: 0 });
        }
        let frames = f.proxy.frames.lock().unwrap();
        assert!(frames.iter().any(|frame| matches!(frame, Frame::Have { git_dependencies, .. } if git_dependencies == &["https://example.invalid/private.git"])));
        let manifests = frames
            .iter()
            .find_map(|frame| match frame {
                Frame::Run(r) => {
                    assert!(r.git_dependencies.is_empty());
                    Some(&r.bundles)
                }
                Frame::Prepare(p) => {
                    assert!(p.git_dependencies.is_empty());
                    Some(&p.bundles)
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
}
