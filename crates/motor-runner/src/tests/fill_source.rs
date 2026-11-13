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
    source_fixture_with_aliases(false)
}

#[test]
fn export_skips_a_tip_dependency_absent_from_the_job_snapshot() {
    let f = Fixture::new();
    let dep = "a".repeat(40);
    fs::write(
        f.main.join("Cargo.lock"),
        format!("version = 3\n[[package]]\nname='dep'\nversion='0.1.0'\nsource='git+https://example.invalid/dependency?rev={dep}#{dep}'\n"),
    )
    .unwrap();
    let (tip, _) = commit(&f.main, "default branch adds a locked dependency");
    fs::remove_file(f.main.join("Cargo.lock")).unwrap();
    commit(&f.main, "job snapshot does not use the dependency");
    let mut config = f.config();
    config.manual_tick = true;
    let runner = Runner::open(config).unwrap();
    let (mut request, bytes) = f.run("true");
    request.integration_tip = tip;
    assert!(request.git_dependencies.is_empty());
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
    runner
        .core
        .trees
        .import_dependencies(&upload.directory, &request.git_dependencies)
        .unwrap();
    assert!(crate::writer::fill::locked_git(
        &runner.core.trees,
        &request.repository_root,
        &request.snapshot_commit,
    )
    .unwrap()
    .is_empty());
    assert!(!runner
        .core
        .trees
        .dependency_mirror("https://example.invalid/dependency")
        .exists());
    let directory = f.scratch.join("unreceived-export");
    let result = crate::fill_source::export(
        &runner.core.trees,
        &request.repository_root,
        &request.integration_tip,
        &directory,
        motor_protocol::FILL_SOURCE_MAX_BYTES,
    );
    assert!(result.is_ok(), "{result:?}");
    let crate::fill_source::Export::Incomplete { url, commit } = result.unwrap() else {
        panic!("a tip-only dependency must skip export");
    };
    assert_eq!(url, "https://example.invalid/dependency");
    assert_eq!(commit, dep);
    assert!(!directory.exists());
}

#[test]
fn export_checks_every_tip_dependency_commit_before_writing_bundles() {
    for non_commit in [false, true] {
        let (f, runner, _, _, source) = source_fixture();
        let dependency = f.scratch.join("dependency");
        let missing = if non_commit {
            git(&dependency, &["rev-parse", "HEAD^{tree}"])
        } else {
            commit(&dependency, "revision the job never received").0
        };
        let present = &source.git_dependencies[0].commits[0];
        fs::write(
            f.main.join("Cargo.lock"),
            format!("version = 3\n[[package]]\nname='dep'\nversion='0.1.0'\nsource='git+https://example.invalid/dependency#{present}'\n[[package]]\nname='dep'\nversion='0.2.0'\nsource='git+https://example.invalid/dependency?rev={missing}#{missing}'\n"),
        )
        .unwrap();
        commit(&f.main, "default branch changes dependency revision");
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
        let directory = f.scratch.join("incomplete-export");
        let exported = crate::fill_source::export(
            &runner.core.trees,
            &request.repository_root,
            &request.integration_tip,
            &directory,
            motor_protocol::FILL_SOURCE_MAX_BYTES,
        )
        .unwrap();
        let crate::fill_source::Export::Incomplete { url, commit } = exported else {
            panic!("missing or non-commit dependency must skip export");
        };
        assert_eq!(url, "https://example.invalid/dependency");
        assert_eq!(commit, missing);
        assert!(!directory.exists());
        assert_eq!(
            git(
                &runner.core.trees.dependency_mirror(&url),
                &["rev-parse", &format!("refs/ck-motor/dep/{present}")],
            ),
            *present
        );
    }
}

fn source_fixture_with_aliases(
    aliases: bool,
) -> (Fixture, Runner, Run, PathBuf, motor_protocol::FillSource) {
    let f = Fixture::new();
    let dependency = f.scratch.join("dependency");
    fs::create_dir(&dependency).unwrap();
    git(&dependency, &["init"]);
    let (dep, _) = commit(&dependency, "dependency");
    let mut locked = format!("version = 3\n[[package]]\nname='dep'\nversion='0.1.0'\nsource='git+https://example.invalid/dependency#{dep}'\n");
    if aliases {
        let (second, _) = commit(&dependency, "second revision");
        locked.push_str(&format!("[[package]]\nname='dep'\nversion='0.2.0'\nsource='git+https://example.invalid/dependency?rev={second}#{second}'\n[[package]]\nname='alias'\nversion='0.1.0'\nsource='git+https://example.invalid/dependency.git#{dep}'\n"));
    }
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname='source'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(f.main.join("Cargo.lock"), locked).unwrap();
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
    if aliases {
        let alias = runner
            .core
            .trees
            .dependency_mirror("https://example.invalid/dependency.git");
        git(
            &dependency,
            &["clone", "--bare", text(&dependency), text(&alias)],
        );
    }
    let directory = f.scratch.join("export");
    let crate::fill_source::Export::Complete(source) = crate::fill_source::export(
        &runner.core.trees,
        &request.repository_root,
        &request.integration_tip,
        &directory,
        motor_protocol::FILL_SOURCE_MAX_BYTES,
    )
    .unwrap() else {
        panic!("fixture contains all locked dependency commits");
    };
    (f, runner, request, directory, source)
}

#[cfg(target_os = "linux")]
#[test]
fn imported_fill_accepts_lockfile_url_aliases_and_multiple_revisions() {
    let (f, _, _, directory, source) = source_fixture_with_aliases(true);
    let mut config = f.config();
    config.state_root = f.scratch.join("alias-fill-state");
    config.manual_tick = true;
    config.tenant_vm = true;
    config.fill_vm = true;
    config.tenant_write_key = Some(true);
    let runner = Runner::open(config).unwrap();
    let (id, _hold) = runner
        .import_fill_source_contents(&source, &directory)
        .unwrap();
    let id = id.unwrap();
    let inner = runner.core.inner.lock().unwrap();
    let imported = &inner.jobs[&id].request.as_ref().unwrap().git_dependencies;
    let expected: BTreeSet<_> = source
        .git_dependencies
        .iter()
        .flat_map(|d| {
            d.commits
                .iter()
                .map(|commit| (d.url.clone(), commit.clone()))
        })
        .collect();
    assert_eq!(expected.len(), 3);
    assert_eq!(
        imported
            .iter()
            .flat_map(|d| {
                d.commits
                    .iter()
                    .map(|commit| (d.url.clone(), commit.clone()))
            })
            .collect::<BTreeSet<_>>(),
        expected
    );
    for (url, commit) in expected {
        let mirror = runner.core.trees.dependency_mirror(&url);
        assert_eq!(
            git(
                &mirror,
                &["rev-parse", &format!("refs/ck-motor/dep/{commit}")]
            ),
            commit
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn refused_imported_fill_releases_its_source_hold() {
    let (f, _, _, directory, source) = source_fixture();
    let mut config = f.config();
    config.state_root = f.scratch.join("refused-writer-state");
    config.manual_tick = true;
    config.tenant_vm = true;
    config.fill_vm = true;
    config.tenant_write_key = Some(false);
    let runner = Runner::open(config).unwrap();
    let result = runner.import_fill_source_contents(&source, &directory);
    let error = result
        .err()
        .expect("a fill without writer authority must be refused");
    assert!(
        error.to_string().contains("fill writer unavailable"),
        "{error}"
    );
    assert!(runner.core.fill_source_holds.lock().unwrap().is_empty());
    runner.maintenance().unwrap();
    let refs = git(
        &runner.core.trees.mirror(&source.repository_root),
        &["for-each-ref", "--format=%(refname)"],
    );
    assert!(!refs.contains("refs/snapshots/"));
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
            .err()
            .expect("a job boot must refuse fill import");
        assert!(error.to_string().contains("boot kind"), "{error}");
    }
}
#[cfg(target_os = "linux")]
#[test]
fn failed_writer_publication_is_not_reported_as_fill_success() {
    let (f, runner, _, _, _) = source_fixture();
    let id = Uuid::now_v7();
    let error = runner.fill_settled(id, &f.root).unwrap_err();
    assert!(error.to_string().contains("record is missing"), "{error}");
    {
        let mut inner = runner.core.inner.lock().unwrap();
        let repository = inner
            .catalog
            .repositories
            .entry(f.root.clone())
            .or_default();
        repository.result = Some(RebuildResult::Failed);
        repository.failure_detail = Some("cache ACL repair/tracking: read-only file system".into());
    }
    let error = runner.fill_settled(id, &f.root).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cache ACL repair/tracking: read-only file system"),
        "{error}"
    );
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .repositories
        .get_mut(&f.root)
        .unwrap()
        .failure_detail = Some(" ".into());
    let error = runner.fill_settled(id, &f.root).unwrap_err();
    assert!(
        error.to_string().contains("Failed")
            && error.to_string().contains("without a failure detail"),
        "{error}"
    );
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn imported_fill_settles_ok_and_root_policy_publishes_a_fixed_path_base() {
    published_imported_fill(false).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn imported_fill_completion_keeps_snapshot_pins_through_maintenance() {
    published_imported_fill(true).await;
}

#[cfg(target_os = "linux")]
async fn published_imported_fill(maintain: bool) {
    use crate::build_base::{self, CompatibleInputs, FillLaunch, Timestamp, ToolchainIdentity};
    let (f, _tenant, _, directory, source) = source_fixture();
    let saved = f.scratch.join("filled-bd");
    let mut config = f.config();
    config.state_root = f.scratch.join("fill-vm-state");
    config.manual_tick = true;
    config.tenant_vm = true;
    config.fill_vm = true;
    config.tenant_write_key = Some(true);
    config.base_commands = vec![format!(
        "mkdir -p \"$CARGO_TARGET_DIR/bd\" && printf compiled > \"$CARGO_TARGET_DIR/bd/artifact\" && cp -R \"$CARGO_TARGET_DIR/bd\" {}",
        saved.display()
    )];
    let runner = Runner::open(config.clone()).unwrap();
    let (id, hold) = runner
        .import_fill_source_contents(&source, &directory)
        .unwrap();
    let id = id.unwrap();
    if maintain {
        let parent = git(&f.main, &["rev-parse", "HEAD^"]);
        let stale = format!("refs/snapshots/{parent}");
        let mirror = runner.core.trees.mirror(&source.repository_root);
        git(&mirror, &["update-ref", &stale, &parent]);
        runner.maintenance().unwrap();
        assert!(!git(&mirror, &["for-each-ref", "--format=%(refname)"]).contains(&stale));
    }
    runner.tick().unwrap();
    assert_eq!(
        terminal(&runner, id).await.outcome,
        Outcome::Exit { code: 0 }
    );
    until(|| {
        runner
            .fill_settled(id, &source.repository_root)
            .unwrap_or(false)
    })
    .await;
    assert_eq!(fs::read(saved.join("artifact")).unwrap(), b"compiled");

    // The portable command uses the development backend. Exercise the root
    // policy/publication layer with its real output and the existing fixture
    // ownership seam. Real system accounts, unit shutdown and privileged UID
    // transfer are covered by the ignored isolation tests, not this fixture.
    config.isolate_jobs = true;
    config.fixed_path_repositories = vec!["*".into()];
    config.account_subject = Some("acct:01ARZ3NDEKTSV4RRFFQ69G5FAV".into());
    let mut job = runner.core.inner.lock().unwrap().jobs[&id].clone();
    let request = job.request.as_ref().unwrap();
    let checkout = paths::host_run(runner.config(), request).host_cwd;
    let fixed = paths::fixed_paths(&config, request).unwrap().unwrap();
    job.base_launch = Some(FillLaunch {
        inputs: CompatibleInputs {
            toolchain: ToolchainIdentity {
                rustc_verbose: "portable fixture toolchain".into(),
                rust_toolchain: None,
                rust_toolchain_toml: None,
                wrapper: None,
            },
            environment: "portable fixture environment".into(),
        },
        fixed_path: fixed.checkout.clone(),
        reference: Timestamp::reference().unwrap(),
    });
    let generation = build_base::staging(&config.state_dir(), &source.repository_root, id);
    fs::create_dir_all(generation.join("target")).unwrap();
    fs::rename(&saved, generation.join("target/bd")).unwrap();
    build_base::publish_with_fixture_ownership(&config, &runner.core.trees, &checkout, &job)
        .unwrap();
    let record = generation.parent().unwrap().join("current/base.json");
    let record: build_base::Record = serde_json::from_slice(&fs::read(record).unwrap()).unwrap();
    assert_eq!(record.commit, source.integration_tip);
    assert_eq!(record.fixed_path, fixed.checkout);
    assert_eq!(
        fs::read(
            generation
                .parent()
                .unwrap()
                .join("current/target/bd/artifact")
        )
        .unwrap(),
        b"compiled"
    );
    assert!(runner.fill_settled(id, &source.repository_root).unwrap());
    if maintain {
        runner.maintenance().unwrap();
    }
    let reply = crate::fill_source::completion(&runner, id, &source)
        .await
        .unwrap();
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["base_published"], true);
    let pins: Vec<motor_protocol::MirrorRef> =
        serde_json::from_value(reply["have"].clone()).unwrap();
    assert!(pins.iter().any(|pin| pin.reference
        == format!("refs/snapshots/{}", source.integration_tip)
        && pin.commit == source.integration_tip));
    drop(hold);
    if maintain {
        runner.maintenance().unwrap();
        let refs = git(
            &runner.core.trees.mirror(&source.repository_root),
            &["for-each-ref", "--format=%(refname)"],
        );
        assert!(!refs.contains(&format!("refs/snapshots/{}", source.integration_tip)));
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn failed_fill_completion_retains_errors_before_the_progress_tail() {
    let (f, _, _, directory, source) = source_fixture();
    let mut config = f.config();
    config.state_root = f.scratch.join("failed-fill-state");
    config.manual_tick = true;
    config.tenant_vm = true;
    config.fill_vm = true;
    config.tenant_write_key = Some(true);
    config.base_commands = vec!["printf 'error[E0001]: missing fixture symbol\\nwarning: build failed, waiting for other jobs to finish...\\n'; i=0; while [ $i -lt 300 ]; do printf '   Compiling fixture-%s\\n' \"$i\"; i=$((i+1)); done; printf 'final output\\n'; exit 101".into()];
    let runner = Runner::open(config).unwrap();
    let (id, hold) = runner
        .import_fill_source_contents(&source, &directory)
        .unwrap();
    let id = id.unwrap();
    runner.tick().unwrap();
    assert_eq!(
        terminal(&runner, id).await.outcome,
        Outcome::Exit { code: 101 }
    );
    let error = crate::fill_source::completion(&runner, id, &source)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("fill command failed (Exit { code: 101 })"),
        "{error}"
    );
    assert!(
        error.contains("error[E0001]: missing fixture symbol"),
        "{error}"
    );
    assert!(error.contains("warning: build failed"), "{error}");
    assert!(error.ends_with("final output"), "{error}");
    assert!(error.len() <= 2048 + 64, "{} bytes", error.len());
    drop(hold);
    assert!(runner.core.fill_source_holds.lock().unwrap().is_empty());
}
