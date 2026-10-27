use super::*;
use crate::workspace::dependencies::{configure_fetch_git, fetch_command};

const URL: &str = "https://unroutable.invalid/private.git";
fn declaration(url: &str, commits: &[&str]) -> GitDependency {
    GitDependency {
        url: url.into(),
        commits: commits.iter().map(|c| c.to_string()).collect(),
        bundle: None,
    }
}
fn dep_bundle(
    f: &Fixture,
    source: &Path,
    oid: &str,
    index: u32,
    extra: &[&str],
) -> (GitDependency, Vec<u8>) {
    let reference = format!("refs/ck-motor/dep/{oid}");
    git(source, &["update-ref", &reference, oid]);
    let path = f.scratch.join(format!("dep-{index}.bundle"));
    let mut args = vec!["bundle", "create", text(&path), &reference];
    args.extend(extra);
    git(source, &args);
    git(source, &["update-ref", "-d", &reference]);
    let bytes = fs::read(path).unwrap();
    let mut d = declaration(URL, &[oid]);
    d.bundle = Some(BundleManifest {
        index,
        repository: URL.into(),
        length: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    });
    (d, bytes)
}
fn stage(f: &Fixture, d: &GitDependency, bytes: &[u8]) -> PathBuf {
    let directory = f.scratch.join(format!("upload-{}", Uuid::now_v7()));
    fs::create_dir(&directory).unwrap();
    fs::write(
        directory.join(d.bundle.as_ref().unwrap().index.to_string()),
        bytes,
    )
    .unwrap();
    directory
}
fn import(runner: &Runner, directory: &Path, deps: &[GitDependency]) {
    runner
        .core
        .trees
        .validate_dependency_bundles(directory, deps)
        .unwrap();
    runner
        .core
        .trees
        .import_dependencies(directory, deps)
        .unwrap();
}
#[test]
fn dependency_bad_commit_and_local_urls_are_refused() {
    for oid in [
        "ABC",
        &"A".repeat(40),
        &"a".repeat(39),
        &"b".repeat(65),
        "--help",
    ] {
        assert!(workspace::validate_dependencies(&[declaration(URL, &[oid])]).is_err());
    }
    for url in [
        "file:///tmp/repo",
        "https:///",
        "repo",
        "git@host:repo",
        "https://host/repo\n",
        "ext::command",
        "https://user:secret@host/repo",
        "ssh://user:secret@host/repo",
    ] {
        assert!(workspace::validate_dependencies(&[declaration(url, &[&"a".repeat(40)])]).is_err());
    }
    for url in [
        URL,
        "http://host/repo",
        "ssh://git@host/repo",
        "git://host/repo",
    ] {
        assert!(workspace::validate_dependencies(&[declaration(
            url,
            &[&"a".repeat(40), &"b".repeat(64)]
        )])
        .is_ok());
    }
}
#[test]
fn two_ref_dependency_bundle_is_refused_before_any_import() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let oid = git(&f.main, &["rev-parse", "HEAD"]);
    git(&f.main, &["update-ref", "refs/extra", &oid]);
    let (d, bytes) = dep_bundle(&f, &f.main, &oid, 0, &["refs/extra"]);
    let directory = stage(&f, &d, &bytes);
    assert!(runner
        .core
        .trees
        .validate_dependency_bundles(&directory, &[d])
        .is_err());
    assert!(!runner.core.trees.dependency_mirror(URL).exists());
}
#[test]
fn dependency_wrong_ref_annotated_tag_and_prerequisites_are_refused() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let first = git(&f.main, &["rev-parse", "HEAD"]);
    let (mut wrong, bytes) = dep_bundle(&f, &f.main, &first, 0, &[]);
    let directory = stage(&f, &wrong, &bytes);
    wrong.commits = vec!["a".repeat(40)];
    assert!(runner
        .core
        .trees
        .validate_dependency_bundles(&directory, &[wrong])
        .is_err());
    git(
        &f.main,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "tag",
            "-a",
            "v1",
            "-m",
            "tag",
        ],
    );
    let tag = git(&f.main, &["rev-parse", "v1"]);
    let (d, bytes) = dep_bundle(&f, &f.main, &tag, 1, &[]);
    let directory = stage(&f, &d, &bytes);
    assert!(runner
        .core
        .trees
        .validate_dependency_bundles(&directory, &[d])
        .is_err());
    let (second, _) = commit(&f.main, "second\n");
    let excluded = format!("^{first}");
    let (d, bytes) = dep_bundle(&f, &f.main, &second, 2, &[&excluded]);
    let directory = stage(&f, &d, &bytes);
    assert!(runner
        .core
        .trees
        .validate_dependency_bundles(&directory, &[d])
        .is_err());
}
#[test]
fn bundled_entries_require_one_commit_and_no_duplicate_bundle() {
    let f = Fixture::new();
    let oid = git(&f.main, &["rev-parse", "HEAD"]);
    let (d, _) = dep_bundle(&f, &f.main, &oid, 0, &[]);
    assert!(workspace::validate_dependencies(&[d.clone(), d.clone()]).is_err());
    let mut multiple = d.clone();
    multiple.commits.push("a".repeat(40));
    assert!(workspace::validate_dependencies(&[multiple]).is_err());
    let declared = declaration(URL, &[&oid, &"b".repeat(40)]);
    assert!(workspace::validate_dependencies(&[d, declared]).is_ok());
}
#[test]
fn routing_requires_every_commit_across_repeated_url_entries() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let oid = git(&f.main, &["rev-parse", "HEAD"]);
    let (d, bytes) = dep_bundle(&f, &f.main, &oid, 0, &[]);
    let directory = stage(&f, &d, &bytes);
    import(&runner, &directory, std::slice::from_ref(&d));
    let missing = "a".repeat(40);
    let dependencies = [d.clone(), declaration(URL, &[&missing])];
    let (routes, diagnostic) = runner.core.trees.dependency_routes(&dependencies).unwrap();
    assert!(routes.is_empty());
    assert_eq!(
        diagnostic,
        [format!("git dependency {URL} missing commit {missing}")]
    );
    let (second, _) = commit(&f.main, "second\n");
    let (d2, bytes) = dep_bundle(&f, &f.main, &second, 1, &[]);
    let directory = stage(&f, &d2, &bytes);
    import(&runner, &directory, std::slice::from_ref(&d2));
    let (routes, missing) = runner.core.trees.dependency_routes(&[d, d2]).unwrap();
    assert!(routes.contains_key(URL));
    assert!(missing.is_empty());
    let held = runner.have_dependencies(vec![URL.into()]).unwrap();
    assert_eq!(
        held[0].commits.iter().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([oid, second])
    );
    let mirror = runner.core.trees.dependency_mirror(URL);
    assert_eq!(git(&mirror, &["config", "gc.auto"]), "0");
    fs::remove_dir_all(mirror).unwrap();
    assert!(runner.have_dependencies(vec![URL.into()]).unwrap()[0]
        .commits
        .is_empty());
}
#[test]
fn strict_prefix_url_is_never_routed() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let oid = git(&f.main, &["rev-parse", "HEAD"]);
    let (d, bytes) = dep_bundle(&f, &f.main, &oid, 0, &[]);
    let directory = stage(&f, &d, &bytes);
    import(&runner, &directory, std::slice::from_ref(&d));
    let longer = format!("{URL}/extra");
    let (routes, _) = runner
        .core
        .trees
        .dependency_routes(&[d, declaration(&longer, &[&oid])])
        .unwrap();
    assert!(!routes.contains_key(URL));
}

#[test]
fn credential_helper_and_inherited_git_config_cannot_run_during_fetch() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    let f = Fixture::new();
    let account = f.scratch.join("account");
    let empty = f.scratch.join("empty-home");
    fs::create_dir(&account).unwrap();
    fs::create_dir(&empty).unwrap();
    let account_marker = f.scratch.join("account-helper-ran");
    let inherited_marker = f.scratch.join("inherited-helper-ran");
    fs::write(
        account.join(".gitconfig"),
        format!(
            "[credential]\nhelper = \"!echo leaked > '{}'\"\n",
            account_marker.display()
        ),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let remote = format!("http://{}/private.git", listener.local_addr().unwrap());
    let serving = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request);
        stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let before: BTreeMap<_, _> = std::env::vars_os().collect();
    let mut cmd = Command::new("git");
    cmd.args(["ls-remote", &remote])
        .env("HOME", &account)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env(
            "GIT_CONFIG_VALUE_0",
            format!("!echo leaked > '{}'", inherited_marker.display()),
        )
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY");
    configure_fetch_git(&mut cmd, &empty, &BTreeMap::new()).unwrap();
    let result = cmd.output().unwrap();
    serving.join().unwrap();
    assert!(!result.status.success());
    assert!(!account_marker.exists(), "account credential helper ran");
    assert!(
        !inherited_marker.exists(),
        "inherited credential helper ran"
    );
    assert_eq!(before, std::env::vars_os().collect());
    let mut ssh = Command::new("git");
    ssh.args(["ls-remote", "ssh://git@unroutable.invalid/private.git"]);
    configure_fetch_git(&mut ssh, &empty, &BTreeMap::new()).unwrap();
    let result = ssh.output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("transport 'ssh' not allowed"));
}

#[test]
fn git_dependency_fetch_matrix_builds_offline_and_control_without_rewrite_fails() {
    let f = Fixture::new();
    let source = f.scratch.join("tiny-dep");
    fs::create_dir_all(source.join("src")).unwrap();
    git(&source, &["init"]);
    fs::write(
        source.join("Cargo.toml"),
        "[package]\nname = \"tiny_dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(source.join("src/lib.rs"), "pub fn value() -> u32 { 7 }\n").unwrap();
    let (old, _) = commit(&source, "old");
    let (tip, _) = commit(&source, "tip");
    git(&source, &["branch", "feature", &tip]);
    git(&source, &["tag", "v1", &old]);
    let rustup = PathBuf::from(std::env::var_os("RUSTUP_HOME").unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap())
            .join(".rustup")
            .into_os_string()
    }));
    for (i, (selector, manifest_selector, oid)) in [
        ("".to_string(), "".to_string(), tip.clone()),
        (
            "?branch=feature".to_string(),
            ", branch = \"feature\"".to_string(),
            tip.clone(),
        ),
        (
            "?tag=v1".to_string(),
            ", tag = \"v1\"".to_string(),
            old.clone(),
        ),
        (
            format!("?rev={tip}"),
            format!(", rev = \"{tip}\""),
            tip.clone(),
        ),
        (
            format!("?rev={old}"),
            format!(", rev = \"{old}\""),
            old.clone(),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut config = f.config();
        config.state_root = f.scratch.join(format!("matrix-state-{i}"));
        let runner = Runner::open(config).unwrap();
        let (d, bytes) = dep_bundle(&f, &source, &oid, i as u32, &[]);
        let directory = stage(&f, &d, &bytes);
        import(&runner, &directory, std::slice::from_ref(&d));
        let mirror = runner.core.trees.dependency_mirror(URL);
        assert_eq!(
            git(&mirror, &["for-each-ref", "--format=%(refname)"]),
            format!("refs/ck-motor/dep/{oid}")
        );
        let workspace = f.scratch.join(format!("matrix-workspace-{i}"));
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("Cargo.toml"), format!("[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\ntiny_dep = {{ git = \"{URL}\"{manifest_selector} }}\n")).unwrap();
        fs::write(
            workspace.join("src/main.rs"),
            "fn main() { assert_eq!(tiny_dep::value(), 7); }\n",
        )
        .unwrap();
        fs::write(workspace.join("Cargo.lock"), format!("version = 4\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"tiny_dep\"]\n[[package]]\nname = \"tiny_dep\"\nversion = \"0.1.0\"\nsource = \"git+{URL}{selector}#{oid}\"\n")).unwrap();
        let cargo = f.scratch.join(format!("matrix-cargo-{i}"));
        fs::create_dir_all(&cargo).unwrap();
        let (routes, missing) = runner
            .core
            .trees
            .dependency_fetch_routes(
                &[d],
                &fs::read_to_string(workspace.join("Cargo.lock")).unwrap(),
            )
            .unwrap();
        assert!(missing.is_empty());
        runner
            .core
            .trees
            .refresh_dependency_aliases(
                &fs::read_to_string(workspace.join("Cargo.lock")).unwrap(),
                &routes,
            )
            .unwrap();
        let mut fetch = fetch_command(
            &runner.core.trees.state,
            &workspace,
            &routes,
            &cargo,
            &rustup,
        )
        .unwrap();
        fetch.env("CARGO_NET_OFFLINE", "false");
        let result = fetch.output().unwrap();
        assert!(
            result.status.success(),
            "selector {selector}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let result = Command::new("cargo")
            .args(["build", "--locked", "--offline"])
            .current_dir(&workspace)
            .env("CARGO_HOME", &cargo)
            .env("RUSTUP_HOME", &rustup)
            .env("CARGO_TARGET_DIR", workspace.join("target"))
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "offline {selector}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let empty_cargo = f.scratch.join(format!("control-cargo-{i}"));
        fs::create_dir_all(&empty_cargo).unwrap();
        let mut control = fetch_command(
            &runner.core.trees.state,
            &workspace,
            &BTreeMap::new(),
            &empty_cargo,
            &rustup,
        )
        .unwrap();
        control
            .env("CARGO_NET_OFFLINE", "false")
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY");
        let result = control.output().unwrap();
        assert!(
            !result.status.success(),
            "unroutable control unexpectedly fetched {selector}"
        );
    }
}

fn lock_sources(sources: &[String]) -> String {
    sources
        .iter()
        .enumerate()
        .map(|(i, source)| {
            format!("[[package]]\nname = \"p{i}\"\nversion = \"0.1.0\"\nsource = {source:?}\n")
        })
        .collect()
}
#[test]
fn dependency_aliases_refresh_stale_tips_and_refuse_conflicts_and_bad_refs() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let old = git(&f.main, &["rev-parse", "HEAD"]);
    let (d, bytes) = dep_bundle(&f, &f.main, &old, 0, &[]);
    import(&runner, &stage(&f, &d, &bytes), std::slice::from_ref(&d));
    let (new, _) = commit(&f.main, "new");
    let (d2, bytes) = dep_bundle(&f, &f.main, &new, 1, &[]);
    import(&runner, &stage(&f, &d2, &bytes), std::slice::from_ref(&d2));
    let (routes, _) = runner.core.trees.dependency_routes(&[d, d2]).unwrap();
    let branch = |oid: &str| lock_sources(&[format!("git+{URL}?branch=feature%2Fone#{oid}")]);
    runner
        .core
        .trees
        .refresh_dependency_aliases(&branch(&old), &routes)
        .unwrap();
    let mirror = &routes[URL];
    assert_eq!(git(mirror, &["rev-parse", "refs/heads/feature/one"]), old);
    runner
        .core
        .trees
        .refresh_dependency_aliases(&branch(&new), &routes)
        .unwrap();
    assert_eq!(git(mirror, &["rev-parse", "refs/heads/feature/one"]), new);
    let conflicting = lock_sources(&[
        format!("git+{URL}?branch=feature#{old}"),
        format!("git+{URL}?branch=feature#{new}"),
    ]);
    let error = runner
        .core
        .trees
        .refresh_dependency_aliases(&conflicting, &routes)
        .unwrap_err()
        .to_string();
    assert!(error.contains(URL) && error.contains("refs/heads/feature"));
    assert!(runner
        .core
        .trees
        .refresh_dependency_aliases(
            &lock_sources(&[format!("git+{URL}?branch=../invalid#{new}")]),
            &routes
        )
        .is_err());
    runner
        .core
        .trees
        .refresh_dependency_aliases(
            &lock_sources(&[format!("git+{URL}?rev={old}#{old}")]),
            &routes,
        )
        .unwrap();
    assert_eq!(
        git(
            mirror,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/",
                "refs/tags/"
            ]
        ),
        format!("refs/heads/ck-motor-rev/{old}")
    );
    assert_eq!(
        git(mirror, &["symbolic-ref", "HEAD"]),
        format!("refs/heads/ck-motor-rev/{old}")
    );
}

#[test]
fn disk_guard_evicts_idle_dependency_mirrors_not_job_or_prepare_mirrors() {
    let f = Fixture::new();
    let pressure = Arc::new(AtomicBool::new(false));
    let mut config = f.config();
    config.free_space_floor_bytes = 100;
    let idle = config.state_dir().join("git-deps").join(format!(
        "{}.git",
        workspace::name("https://idle.invalid/repo")
    ));
    let flag = pressure.clone();
    let idle_probe = idle.clone();
    config.free_space_probe = Arc::new(move |_| {
        Ok(if flag.load(Ordering::SeqCst) && idle_probe.exists() {
            50
        } else {
            300
        })
    });
    let runner = Runner::open(config).unwrap();
    let oid = git(&f.main, &["rev-parse", "HEAD"]);
    let (d, bytes) = dep_bundle(&f, &f.main, &oid, 0, &[]);
    import(&runner, &stage(&f, &d, &bytes), &[d]);
    fs::create_dir_all(&idle).unwrap();
    fs::write(idle.join("owned"), "idle").unwrap();
    let preparing_url = "https://prepare.invalid/repo";
    let preparing_mirror = runner.core.trees.dependency_mirror(preparing_url);
    fs::create_dir_all(&preparing_mirror).unwrap();
    let (mut run, _) = f.run("true");
    run.git_dependencies = vec![declaration(URL, &[&oid])];
    assert!(matches!(
        runner.accept(run.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    let mut prepare = prepare_for(&run);
    prepare.workspace_key = f
        .scratch
        .join("remote/worktrees/preparing")
        .to_string_lossy()
        .into();
    prepare.git_dependencies = vec![declaration(preparing_url, &[&oid])];
    let _guard = runner.admit_prepare(&prepare).unwrap().unwrap();
    pressure.store(true, Ordering::SeqCst);
    assert!(runner.ensure_disk_space().unwrap().is_none());
    assert!(!idle.exists());
    assert!(runner.core.trees.dependency_mirror(URL).exists());
    assert!(preparing_mirror.exists());
}

#[tokio::test]
async fn run_and_prepare_transfer_dependency_chunks_after_workspace_and_sibling() {
    for preparing in [false, true] {
        let f = Fixture::new();
        let runner = Runner::open(f.config()).unwrap();
        let (socket, server) = listener(&f, &runner);
        let mut stream = connect(&socket).await;
        write_frame_async(
            &mut stream,
            &Frame::Have {
                repositories: vec![],
                git_dependencies: vec![URL.into()],
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(read_next(&mut stream).await, Frame::HaveReply { git_dependencies: Some(held), .. } if held[0].commits.is_empty())
        );
        let (mut run, mut bytes) = f.run("true");
        let oid = git(&f.main, &["rev-parse", "HEAD"]);
        let (dependency, bundle) = dep_bundle(&f, &f.main, &oid, 2, &[]);
        run.git_dependencies = vec![dependency];
        bytes.push(bundle);
        if preparing {
            let mut prepare = prepare_for(&run);
            prepare.bundles = run.bundles.clone();
            prepare.git_dependencies = run.git_dependencies.clone();
            write_frame_async(&mut stream, &Frame::Prepare(prepare))
                .await
                .unwrap();
            assert!(matches!(
                read_next(&mut stream).await,
                Frame::PrepareAccepted { .. }
            ));
            chunks(&mut stream, &bytes).await;
            assert!(matches!(
                read_next(&mut stream).await,
                Frame::Prepared { .. }
            ));
        } else {
            write_frame_async(&mut stream, &Frame::Run(run))
                .await
                .unwrap();
            assert!(matches!(read_next(&mut stream).await, Frame::Accepted(_)));
            chunks(&mut stream, &bytes).await;
            loop {
                if let Frame::Terminal(terminal) = read_next(&mut stream).await {
                    assert_eq!(terminal.outcome, Outcome::Exit { code: 0 });
                    break;
                }
            }
        }
        assert_eq!(
            runner.have_dependencies(vec![URL.into()]).unwrap()[0].commits,
            vec![oid]
        );
        server.abort();
    }
}

#[test]
fn dependency_chunks_share_index_length_and_digest_validation() {
    let f = Fixture::new();
    let runner = Runner::open(f.config()).unwrap();
    let (run, bytes) = f.run("true");
    let oid = git(&f.main, &["rev-parse", "HEAD"]);
    let (d, bundle) = dep_bundle(&f, &f.main, &oid, 2, &[]);
    let mut manifests = run.bundles;
    manifests.push(d.bundle.unwrap());
    let mut duplicate = manifests.clone();
    duplicate[2].index = 1;
    assert!(Upload::new(runner.spool_dir(Uuid::now_v7()), &duplicate).is_err());
    for corrupt in [false, true] {
        let mut upload = Upload::new(runner.spool_dir(Uuid::now_v7()), &manifests).unwrap();
        for (index, bytes) in bytes.iter().enumerate() {
            upload
                .chunk(Frame::BundleChunk {
                    index: index as u32,
                    offset: 0,
                    data: BytePayload(bytes.clone()),
                })
                .unwrap();
        }
        let mut data = bundle.clone();
        if corrupt {
            data[0] ^= 1;
        } else {
            data.push(0);
        }
        assert!(upload
            .chunk(Frame::BundleChunk {
                index: 2,
                offset: 0,
                data: BytePayload(data)
            })
            .is_err());
    }
}
