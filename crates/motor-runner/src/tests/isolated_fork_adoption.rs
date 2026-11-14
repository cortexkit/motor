use super::fork_adoption::next_boot;
use super::isolated_targets::{base_store_config, base_store_runner, drive_base_store};
use super::*;

const BUILD: &str = "cargo build --workspace --locked --offline --message-format=json";

struct AdoptionFixture {
    inner: Fixture,
    mounts: Vec<PathBuf>,
}

impl std::ops::Deref for AdoptionFixture {
    type Target = Fixture;

    fn deref(&self) -> &Fixture {
        &self.inner
    }
}

impl Drop for AdoptionFixture {
    fn drop(&mut self) {
        // Stop units and release their source binds before unmounting. The
        // inner fixture removes its scratch tree only after these mounts go.
        assert!(
            self.inner.cleanup.finish(),
            "adoption fixture did not quiesce"
        );
        for target in self.mounts.iter().rev() {
            let output = Command::new("/usr/bin/umount")
                .arg(target)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "unmount {}: {}",
                target.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

fn save(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn snapshot(f: &Fixture) {
    git(&f.main, &["add", "-A"]);
    git(
        &f.main,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "adoption source",
        ],
    );
}

fn fixture() -> (AdoptionFixture, Config) {
    assert_eq!(unsafe { libc::geteuid() }, 0, "operator-only fixture");
    let mut f = AdoptionFixture {
        inner: Fixture::new(),
        mounts: Vec::new(),
    };
    save(&f.main, ".gitignore", "/target/\n");
    save(
        &f.main,
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
    );
    save(&f.main, "Cargo.lock", "version = 4\n\n[[package]]\nname = \"adopt-a\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"adopt-b\"\nversion = \"0.1.0\"\n");
    for name in ["a", "b"] {
        save(
            &f.main,
            &format!("{name}/Cargo.toml"),
            &format!(
                "[package]\nname = \"adopt-{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"
            ),
        );
    }
    save(&f.main, "a/src/lib.rs", "pub fn value() -> u32 { 1 }\n");
    save(
        &f.main,
        "b/src/lib.rs",
        "pub fn value() -> &'static str { include_str!(\"input\") }\n",
    );
    save(&f.main, "b/src/input", "first\n");
    snapshot(&f);
    let mut config = base_store_config(&f, BUILD);
    config.tenant_vm = true;
    config.per_job = true;
    config.image_cargo = f.scratch.join("image-cargo");
    let cargo_home = f.scratch.join("cargo-home");
    config.cargo_home = Some(cargo_home.clone());
    fs::create_dir_all(cargo_home.join("bin")).unwrap();
    // Keep only tool links in this Cargo home. The toolchain itself is already
    // mounted read-only from RUSTUP_HOME, never from the operator's Cargo cache.
    for tool in ["cargo", "rustc", "rustdoc"] {
        let output = Command::new("rustup")
            .args(["which", tool])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        assert!(path.starts_with(std::env::var_os("RUSTUP_HOME").unwrap()));
        std::os::unix::fs::symlink(path, cargo_home.join("bin").join(tool)).unwrap();
    }
    for name in ["registry", "git"] {
        let lower = config.image_cargo.join(name);
        fs::create_dir_all(&lower).unwrap();
        let output = Command::new("/usr/bin/setfacl")
            .args([
                "-m",
                "u:motor-job:r-x,m::r-x,d:u:motor-job:r-x,d:m::r-x",
                "--",
            ])
            .arg(&lower)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // The tenant agent normally mounts these two overlays before Runner::open.
    // Reproduce that layout only under the fixture's own Cargo home.
    for layer in crate::isolation::cargo_sources::layers(&cargo_home, &config.image_cargo) {
        for path in [&layer.upper, &layer.work, &layer.target] {
            fs::create_dir_all(path).unwrap();
        }
        let output = Command::new("/usr/bin/mount")
            .args(["-t", "overlay", "-o", &layer.options(), "overlay"])
            .arg(&layer.target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        f.mounts.push(layer.target);
    }
    // Relocate the checkout disk and per-boot marker as well, without replacing
    // the real root, isolation, account or Cargo ACL checks.
    config.fixture_adoption_roots = Some(f.scratch.join("remote"));
    config.fixture_workspace_forked_marker = Some(f.scratch.join("forked"));
    (f, config)
}

async fn run(f: &Fixture, runner: &Runner, key: &str, command: &str) -> String {
    let (mut request, mut bytes) = f.run_worktree(command);
    request.workspace_key = key.into();
    request.cwd = key.into();
    request.siblings.clear();
    request.bundles.truncate(1);
    bytes.truncate(1);
    f.submit(runner, &request, &bytes);
    drive_base_store(runner, || {
        let inner = runner.core.inner.lock().unwrap();
        inner.jobs[&request.job_id].terminal.is_some() && inner.finalizing.is_empty()
    })
    .await;
    let output = job_output(runner, request.job_id);
    assert_eq!(
        runner.core.inner.lock().unwrap().jobs[&request.job_id]
            .terminal
            .as_ref()
            .unwrap()
            .outcome,
        Outcome::Exit { code: 0 },
        "{output}"
    );
    output
}

fn compiled(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            (value["reason"] == "compiler-artifact" && value["fresh"] == false)
                .then(|| value["target"]["name"].as_str().unwrap().to_string())
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires root, systemd, motor-cache/motor-job accounts and reflink scratch"]
async fn isolated_fork_adoption_fixed_path_build_folder_hash_compiles_zero_crates() {
    let (f, config) = fixture();
    let donor = base_store_runner(config.clone());
    assert_eq!(
        compiled(&run(&f, &donor, &f.key, BUILD).await),
        BTreeSet::from(["adopt_a".into(), "adopt_b".into()])
    );
    drop(donor);
    next_boot(&config, true);
    let successor = base_store_runner(config);
    let key = format!("{}/.worktrees/y", f.key);
    let output = run(&f, &successor, &key, BUILD).await;
    assert!(
        compiled(&output).is_empty(),
        "fixed-path build-folder hash changed: {output}"
    );
    let inner = successor.core.inner.lock().unwrap();
    assert!(inner.catalog.workspaces.contains_key(&key));
    assert!(!inner.catalog.workspaces.contains_key(&f.key));
}

#[tokio::test]
#[ignore = "requires root, systemd, motor-cache/motor-job accounts and reflink scratch"]
async fn isolated_fork_adoption_rebuilds_crate_the_donor_skipped() {
    let (f, config) = fixture();
    let donor = base_store_runner(config.clone());
    run(&f, &donor, &f.key, BUILD).await;
    drop(donor);
    next_boot(&config, false);
    save(&f.main, "b/src/input", "second\n");
    snapshot(&f);
    let donor = base_store_runner(config.clone());
    let partial = run(
        &f,
        &donor,
        &f.key,
        "cargo build -p adopt-a --locked --offline --message-format=json",
    )
    .await;
    assert!(compiled(&partial).is_empty(), "{partial}");
    drop(donor);
    next_boot(&config, true);
    let successor = base_store_runner(config);
    let key = f.scratch.join("remote/worktrees/y");
    let output = run(
        &f,
        &successor,
        text(&key),
        "cargo build -p adopt-b --locked --offline --message-format=json",
    )
    .await;
    assert_eq!(
        compiled(&output),
        BTreeSet::from(["adopt_b".into()]),
        "{output}"
    );
}

#[tokio::test]
#[ignore = "requires root, systemd, motor-cache/motor-job accounts and reflink scratch"]
async fn isolated_fork_adoption_changed_tree_removes_donor_untracked_files() {
    let (f, config) = fixture();
    let donor = base_store_runner(config.clone());
    run(&f, &donor, &f.key, "printf leftover > donor-junk").await;
    assert!(Path::new(&f.key).join("donor-junk").exists());
    drop(donor);
    next_boot(&config, true);
    save(&f.main, "b/src/input", "changed\n");
    snapshot(&f);
    let successor = base_store_runner(config);
    let key = f.scratch.join("remote/worktrees/y");
    run(&f, &successor, text(&key), "test ! -e donor-junk").await;
    assert!(!key.join("donor-junk").exists());
}

#[tokio::test]
#[ignore = "requires root, systemd, motor-cache/motor-job accounts and reflink scratch"]
async fn isolated_fork_adoption_without_marker_stays_cold() {
    let (f, config) = fixture();
    let donor = base_store_runner(config.clone());
    run(&f, &donor, &f.key, BUILD).await;
    drop(donor);
    next_boot(&config, false);
    let successor = base_store_runner(config);
    let key = f.scratch.join("remote/worktrees/y");
    let output = run(&f, &successor, text(&key), BUILD).await;
    assert_eq!(
        compiled(&output),
        BTreeSet::from(["adopt_a".into(), "adopt_b".into()]),
        "{output}"
    );
    assert!(successor
        .core
        .inner
        .lock()
        .unwrap()
        .catalog
        .workspaces
        .contains_key(&f.key));
    assert!(Path::new(&f.key).join("target").is_dir());
}
