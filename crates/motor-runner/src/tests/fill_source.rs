use super::*;

fn rewrite_heads(
    directory: &Path,
    source: &mut motor_protocol::FillSource,
    heads: &[(&str, &str)],
) {
    use sha2::{Digest, Sha256};
    let path = directory.join("0");
    let bytes = fs::read(&path).unwrap();
    let version_end = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    let pack_start = bytes.windows(2).position(|bytes| bytes == b"\n\n").unwrap() + 2;
    let mut edited = bytes[..version_end].to_vec();
    for (oid, reference) in heads {
        edited.extend_from_slice(format!("{oid} {reference}\n").as_bytes());
    }
    edited.push(b'\n');
    edited.extend_from_slice(&bytes[pack_start..]);
    fs::write(&path, &edited).unwrap();
    source.bundles[0].length = edited.len() as u64;
    source.bundles[0].sha256 = format!("{:x}", Sha256::digest(&edited));
}

fn refuse_adversarial_heads(glob: bool) {
    let (f, _, _, directory, mut source) = source_fixture();
    let payload = git(&f.main, &["rev-parse", "HEAD^"]);
    let injected = if glob {
        "refs/ck-motor/*".into()
    } else {
        format!(
            "refs/ck-motor/payload:refs/replace/{}",
            source.integration_tip
        )
    };
    rewrite_heads(
        &directory,
        &mut source,
        &[(&payload, "refs/ck-motor/payload"), (&payload, &injected)],
    );
    let heads = git(
        &f.main,
        &["bundle", "list-heads", text(&directory.join("0"))],
    );
    assert!(
        heads.contains(&injected),
        "Git must expose the hand-edited bundle header: {heads}"
    );
    let mut config = f.config();
    config.state_root = f.scratch.join("adversarial-state");
    config.manual_tick = true;
    let fresh = Runner::open(config).unwrap();
    let result = crate::fill_source::import(&fresh.core.trees, &source, &directory);
    assert!(
        result.is_err(),
        "accepted adversarial bundle head {injected}"
    );
    let error = result.unwrap_err();
    assert!(
        error.to_string().contains("invalid bundle refs"),
        "malformed bundle heads must be refused before Git fetch: {error}"
    );
    let mirror = fresh.core.trees.mirror(&source.repository_root);
    if mirror.exists() {
        assert!(git(&mirror, &["for-each-ref", "--format=%(refname)"]).is_empty());
    }
    assert!(!fresh.config().state_dir().join("build-bases").exists());
}

#[test]
fn colon_bearing_fill_bundle_header_is_refused_without_creating_refs() {
    refuse_adversarial_heads(false);
}

#[test]
fn glob_bearing_fill_bundle_header_is_refused_without_creating_refs() {
    refuse_adversarial_heads(true);
}

#[test]
fn fill_bundle_requires_one_fixed_head_at_the_expected_tip_and_no_replace_refs() {
    let (f, _, _, directory, source) = source_fixture();
    let original = fs::read(directory.join("0")).unwrap();
    let parent = git(&f.main, &["rev-parse", "HEAD^"]);
    for (case, heads) in [
        (
            "wrong-tip",
            vec![(parent.as_str(), "refs/ck-motor/fill-export")],
        ),
        (
            "wrong-name",
            vec![(source.integration_tip.as_str(), "refs/ck-motor/other")],
        ),
        (
            "two-heads",
            vec![
                (source.integration_tip.as_str(), "refs/ck-motor/fill-export"),
                (parent.as_str(), "refs/ck-motor/other"),
            ],
        ),
    ] {
        fs::write(directory.join("0"), &original).unwrap();
        let mut edited = source.clone();
        rewrite_heads(&directory, &mut edited, &heads);
        let mut config = f.config();
        config.state_root = f.scratch.join(case);
        config.manual_tick = true;
        let fresh = Runner::open(config).unwrap();
        assert!(
            crate::fill_source::import(&fresh.core.trees, &edited, &directory).is_err(),
            "{case}"
        );
    }
    fs::write(directory.join("0"), original).unwrap();
    let mut config = f.config();
    config.state_root = f.scratch.join("replacement-state");
    config.manual_tick = true;
    let fresh = Runner::open(config).unwrap();
    crate::fill_source::import(&fresh.core.trees, &source, &directory).unwrap();
    git(
        &fresh.core.trees.mirror(&source.repository_root),
        &[
            "update-ref",
            &format!("refs/replace/{}", source.integration_tip),
            &parent,
        ],
    );
    let error = crate::fill_source::import(&fresh.core.trees, &source, &directory).unwrap_err();
    assert!(error.to_string().contains("replacement refs"), "{error}");
}

#[test]
fn unlocked_dependency_urls_and_commits_are_refused_before_any_dependency_import() {
    use sha2::{Digest, Sha256};
    let (f, _, _, directory, source) = source_fixture();
    let dependency = f.scratch.join("dependency");
    let (unlocked, _) = commit(&dependency, "unlocked");
    for extra_url in [true, false] {
        let mut edited = source.clone();
        let mut extra = edited.git_dependencies[0].clone();
        let bundle = if extra_url {
            extra.url = "https://example.invalid/unlocked".into();
            fs::read(directory.join("1")).unwrap()
        } else {
            extra.commits = vec![unlocked.clone()];
            let reference = format!("refs/ck-motor/dep/{unlocked}");
            git(&dependency, &["update-ref", &reference, &unlocked]);
            let path = f.scratch.join("unlocked.bundle");
            git(&dependency, &["bundle", "create", text(&path), &reference]);
            fs::read(path).unwrap()
        };
        let manifest = BundleManifest {
            index: 2,
            repository: extra.url.clone(),
            length: bundle.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bundle)),
        };
        fs::write(directory.join("2"), bundle).unwrap();
        extra.bundle = Some(manifest.clone());
        edited.git_dependencies.push(extra.clone());
        edited.bundles.push(manifest);
        let mut config = f.config();
        config.state_root = f.scratch.join(if extra_url {
            "extra-url"
        } else {
            "extra-commit"
        });
        config.manual_tick = true;
        let fresh = Runner::open(config).unwrap();
        let error = crate::fill_source::import(&fresh.core.trees, &edited, &directory).unwrap_err();
        assert!(error.to_string().contains("tip lockfile"), "{error}");
        for dependency in &edited.git_dependencies {
            assert!(!fresh.core.trees.dependency_mirror(&dependency.url).exists());
        }
        assert!(!fresh.config().state_dir().join("build-bases").exists());
    }
}

fn source_fixture() -> (Fixture, Runner, Run, PathBuf, motor_protocol::FillSource) {
    let f = Fixture::new();
    let dependency = f.scratch.join("dependency");
    fs::create_dir(&dependency).unwrap();
    git(&dependency, &["init"]);
    let (dep, _) = commit(&dependency, "dependency");
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname='source'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(f.main.join("Cargo.lock"), format!("version = 3\n[[package]]\nname='dep'\nversion='0.1.0'\nsource='git+https://example.invalid/dependency#{dep}'\n")).unwrap();
    commit(&f.main, "second");
    let mut config = f.config();
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let (request, bytes) = f.run("true");
    let upload = f.spool(&runner, &request, &bytes);
    runner
        .core
        .trees
        .fetch(
            &upload.directory,
            &request.bundles,
            &request.repository_root,
            &request.snapshot_commit,
            &request.integration_tip,
            &request.siblings,
        )
        .unwrap();
    let mirror = runner
        .core
        .trees
        .dependency_mirror("https://example.invalid/dependency");
    fs::create_dir_all(mirror.parent().unwrap()).unwrap();
    git(
        &dependency,
        &["clone", "--bare", text(&dependency), text(&mirror)],
    );
    git(
        &mirror,
        &["update-ref", &format!("refs/ck-motor/dep/{dep}"), &dep],
    );
    let directory = f.scratch.join("export");
    let source = crate::fill_source::export(
        &runner.core.trees,
        &request.repository_root,
        &request.integration_tip,
        &directory,
        motor_protocol::FILL_SOURCE_MAX_BYTES,
    )
    .unwrap();
    (f, runner, request, directory, source)
}

#[test]
fn exported_tip_on_a_fresh_disk_has_the_tenant_writer_plan_and_dependency_mirrors() {
    let (f, tenant, request, directory, source) = source_fixture();
    let mut config = f.config();
    config.state_root = f.scratch.join("fresh-state");
    config.manual_tick = true;
    let fresh = Runner::open(config).unwrap();
    assert!(!fresh.core.trees.mirror(&request.repository_root).exists());
    crate::fill_source::import(&fresh.core.trees, &source, &directory).unwrap();
    let original = tenant.base_plan(&request.repository_root, &request.integration_tip, true);
    let imported = fresh.base_plan(&source.repository_root, &source.integration_tip, true);
    assert_eq!(original.tip, imported.tip);
    assert_eq!(original.command, imported.command);
    assert_eq!(original.descriptor.unwrap(), imported.descriptor.unwrap());
    let original = original.fill.unwrap();
    let imported = imported.fill.unwrap();
    assert_eq!(
        original.tree.unwrap().unwrap(),
        imported.tree.unwrap().unwrap()
    );
    assert_eq!(
        original.git_dependencies.unwrap(),
        imported.git_dependencies.unwrap()
    );
    assert_eq!(
        git(
            &fresh.core.trees.mirror(&source.repository_root),
            &["rev-list", "--count", &source.integration_tip]
        ),
        "2"
    );
    for dependency in &source.git_dependencies {
        let mirror = fresh.core.trees.dependency_mirror(&dependency.url);
        assert_eq!(
            git(&mirror, &["cat-file", "-t", &dependency.commits[0]]),
            "commit"
        );
        assert_eq!(
            git(
                &tenant.core.trees.dependency_mirror(&dependency.url),
                &[
                    "rev-parse",
                    &format!("refs/ck-motor/dep/{}", dependency.commits[0])
                ]
            ),
            dependency.commits[0]
        );
    }
    assert!(!fresh.config().state_dir().join("build-bases").exists());
    let pins = crate::fill_source::pins(&fresh.core.trees, &source).unwrap();
    assert!(pins.iter().any(|pin| pin.repository.as_deref()
        == Some(source.repository_root.as_str())
        && pin.reference == format!("refs/tips/{}", source.integration_tip)));
    assert!(pins.iter().any(|pin| pin.dependency.is_some()));
}

#[test]
fn oversized_corrupt_and_missing_tip_sources_never_publish_a_base() {
    use sha2::{Digest, Sha256};
    let (f, tenant, _, directory, source) = source_fixture();
    let mut config = f.config();
    config.state_root = f.scratch.join("refused-state");
    config.manual_tick = true;
    let fresh = Runner::open(config).unwrap();
    let mut oversized = source.clone();
    oversized.bundles[0].length = motor_protocol::FILL_SOURCE_MAX_BYTES + 1;
    assert!(crate::fill_source::import(&fresh.core.trees, &oversized, &directory).is_err());
    assert!(crate::fill_source::export(
        &tenant.core.trees,
        &source.repository_root,
        &source.integration_tip,
        &f.scratch.join("too-large"),
        1
    )
    .is_err());
    let mut missing = source.clone();
    missing.integration_tip = "f".repeat(40);
    assert!(crate::fill_source::import(&fresh.core.trees, &missing, &directory).is_err());
    fs::write(directory.join("0"), b"corrupt git bundle").unwrap();
    assert!(crate::fill_source::import(&fresh.core.trees, &source, &directory).is_err());
    let mut corrupt = source.clone();
    corrupt.bundles[0].length = 18;
    corrupt.bundles[0].sha256 = format!("{:x}", Sha256::digest(b"corrupt git bundle"));
    assert!(crate::fill_source::import(&fresh.core.trees, &corrupt, &directory).is_err());
    assert!(!fresh.config().state_dir().join("build-bases").exists());
}

#[test]
fn helper_fill_boot_refuses_run_and_job_boot_refuses_fill_before_import() {
    assert!(crate::fill_source::run_refusal_for_boot("console=ttyS0 motor.fill=1").is_some());
    assert!(crate::fill_source::run_refusal_for_boot("motor.per_job=1").is_none());
    assert!(crate::fill_source::run_refusal_for_boot("untrusted=motor.fill=1").is_none());
    #[cfg(target_os = "linux")]
    {
        let (f, runner, _, _, source) = source_fixture();
        assert!(
            !crate::fill_source::fill_boot(&fs::read_to_string("/proc/cmdline").unwrap()),
            "portable test must run outside a Fill VM"
        );
        let error = runner
            .import_fill_source(&source, &f.scratch.join("absent"))
            .unwrap_err();
        assert!(error.to_string().contains("boot kind"), "{error}");
    }
}
#[cfg(target_os = "linux")]
#[test]
fn failed_writer_publication_is_not_reported_as_fill_success() {
    let (f, runner, _, _, _) = source_fixture();
    let id = Uuid::now_v7();
    assert!(runner.fill_settled(id, &f.root).is_err());
    {
        let mut inner = runner.core.inner.lock().unwrap();
        let repository = inner
            .catalog
            .repositories
            .entry(f.root.clone())
            .or_default();
        repository.result = Some(RebuildResult::Failed);
    }
    assert!(runner.fill_settled(id, &f.root).is_err());
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .repositories
        .get_mut(&f.root)
        .unwrap()
        .result = Some(RebuildResult::Ok);
    assert!(runner.fill_settled(id, &f.root).unwrap());
}
