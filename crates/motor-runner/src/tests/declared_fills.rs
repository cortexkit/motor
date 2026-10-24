use super::*;

/// Write `.motor/cache.toml` into the fixture's main repository and commit it.
///
/// The fixture ignores `.motor/` the way a real repository may, so the file is
/// staged on purpose; a declaration that is not tracked is not part of a tip.
fn commit_declaration(f: &Fixture, text: &str) -> String {
    fs::create_dir_all(f.main.join(".motor")).unwrap();
    fs::write(f.main.join(declaration::PATH), text).unwrap();
    // Stage the source tree first: `git add .` drops the ignored declaration
    // from the index, so the forced add has to come after it.
    git(&f.main, &["add", "."]);
    git(&f.main, &["add", "-f", declaration::PATH]);
    git(
        &f.main,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "declaration",
        ],
    );
    git(&f.main, &["rev-parse", "HEAD"])
}

use crate::workspace::warm_steps;
use crate::writer::declaration::{self, Compilers};
use crate::writer::fill::{dependency_free, descriptor};

#[test]
fn a_descriptor_changes_with_its_declared_steps_and_matched_files() {
    let f = Fixture::new();
    fs::write(f.main.join("Makefile"), "all:\n\tcc src/main.c\n").unwrap();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/main.c"), "int main(void){return 0;}\n").unwrap();
    let declare = |stanza: &str| commit_declaration(&f, stanza);
    let base = "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"src/**\", \"Makefile\"]\n";
    let compilers = Compilers::default();
    let of = |tip: &str| {
        let declaration = declaration::read(&f.main, tip).unwrap();
        descriptor(
            &f.main,
            tip,
            "rustc A",
            "make",
            declaration.as_ref(),
            &compilers,
        )
        .unwrap()
    };
    let first_tip = declare(base);
    let first = of(&first_tip);
    // Rewriting the same bytes would make an empty commit; a comment makes a
    // new commit of the same declaration.
    let second_tip = declare(&format!("{base}# a new commit of the same declaration\n"));
    assert_ne!(first_tip, second_tip, "two commits were the same tree");
    assert_eq!(
        first,
        of(&second_tip),
        "the same declaration hashed differently"
    );
    // An edit to a file the declaration lists is a different fill.
    fs::write(f.main.join("src/main.c"), "int main(void){return 1;}\n").unwrap();
    let source = commit(&f.main, "source edit").0;
    assert_ne!(
        first,
        of(&source),
        "a listed input did not move the descriptor"
    );
    // A file it does not list is not, even far outside Cargo's inputs.
    fs::create_dir_all(f.main.join("docs")).unwrap();
    fs::write(f.main.join("docs/readme.md"), "notes\n").unwrap();
    let docs = commit(&f.main, "unlisted edit").0;
    assert_eq!(
        of(&source),
        of(&docs),
        "an unlisted file moved the declared descriptor"
    );
    // So are the tool id, the commands and the patterns.
    for changed in [
        base.replace("ccache", "sccache"),
        base.replace("[\"make\"]", "[\"make\", \"make check\"]"),
        base.replace(
            "[\"src/**\", \"Makefile\"]",
            "[\"src/main.c\", \"Makefile\"]",
        ),
    ] {
        let tip = declare(&changed);
        assert_ne!(first, of(&tip), "{changed} did not move the descriptor");
    }
}

#[test]
fn a_compiler_version_changes_a_declared_descriptor_only() {
    let f = Fixture::new();
    fs::write(f.main.join("Makefile"), "all:\n\ttrue\n").unwrap();
    let tip = commit_declaration(
        &f,
        "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"Makefile\"]\n",
    );
    let declaration = declaration::read(&f.main, &tip).unwrap();
    let of = |compilers: &Compilers| {
        descriptor(
            &f.main,
            &tip,
            "rustc A",
            "make",
            declaration.as_ref(),
            compilers,
        )
        .unwrap()
    };
    let mut compilers = Compilers::default();
    let first = of(&compilers);
    compilers
        .versions
        .insert("cc".into(), "cc (Ubuntu 13.2.0) 13.2.0".into());
    let second = of(&compilers);
    assert_ne!(
        first, second,
        "the compiler version did not move the descriptor"
    );
    compilers
        .versions
        .insert("ccache".into(), "ccache version 4.10".into());
    assert_ne!(second, of(&compilers));
    // Without a declaration the compiler plays no part, as before.
    let plain = workspace::git(&f.main, &["rev-parse", "HEAD"]).unwrap();
    let bare = descriptor(&f.main, &plain, "rustc A", "make", None, &compilers).unwrap();
    let probe = Compilers::probe(&["definitely-not-a-compiler"]);
    assert_eq!(
        bare,
        descriptor(&f.main, &plain, "rustc A", "make", None, &probe).unwrap(),
        "a Cargo-only descriptor depends on the compiler"
    );
}

#[test]
fn probing_an_absent_compiler_records_nothing() {
    let probed = Compilers::probe(&["definitely-not-a-compiler-here"]);
    assert!(probed.versions.is_empty());
}

/// A tree with no root `Cargo.toml` has no Cargo warm command of its own, so
/// whatever it declares is the whole warm plan; with nothing declared the plan
/// stays `true` and takes the dependency-free shortcut.
#[test]
fn a_tree_without_cargo_warms_from_its_declaration() {
    let f = Fixture::new();
    let bare = git(&f.main, &["rev-parse", "HEAD"]);
    assert_eq!(workspace::warm_plan(&f.main, &bare), ("true".into(), false));
    let declared = commit_declaration(
        &f,
        "[[fill]]\ntool = \"ccache\"\ncommands = [\"make clean\", \"make -j2\"]\n\"descriptor-inputs\" = [\"src/**\"]\n\n[[fill]]\ntool = \"ccache\"\ncommands = [\"make check\"]\n\"descriptor-inputs\" = [\"tests/**\"]\n",
    );
    assert_eq!(
        workspace::warm_plan(&f.main, &declared),
        ("make clean && make -j2 && make check".to_string(), true),
        "a declared fill is the warm plan, and it is explicit"
    );
    // A Cargo tree runs Cargo's own warm command first and the declared steps
    // after it, in the same fill.
    fs::write(
        f.main.join("Cargo.toml"),
        "[workspace]\n[workspace.metadata.motor]\nwarm-commands = [\"cargo build\"]\n",
    )
    .unwrap();
    let cargo = commit(&f.main, "cargo tree").0;
    assert_eq!(
        workspace::warm_plan(&f.main, &cargo),
        (
            "cargo build && make clean && make -j2 && make check".to_string(),
            true
        )
    );
}

/// A declared fill always runs. The shortcut that records a fill without a
/// writer exists for graphs whose units sccache cannot store, and a
/// declaration is a request to run something.
#[test]
fn a_declared_fill_is_never_skipped_as_dependency_free() {
    let f = Fixture::new();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/main.c"), "int main(void){return 0;}\n").unwrap();
    let tip = commit_declaration(
        &f,
        "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"src/**\"]\n",
    );
    let declared = declaration::read(&f.main, &tip).unwrap();
    assert!(!dependency_free(&f.main, &tip, declared.as_ref()).unwrap());
    assert!(dependency_free(&f.main, &tip, None).unwrap());
}

/// The writer shell runs Cargo's offline preflight only where a Cargo tree is
/// compiled, so a no-Cargo fill is not skipped for lacking a manifest. The
/// preflight itself is unchanged: a Cargo tree whose path dependency lives
/// outside the repository is still refused, as it is today.
#[test]
fn offline_preflight_skips_cargo_only_without_a_manifest() {
    let f = Fixture::new();
    // No Cargo.toml: the shell has nothing for Cargo to check, and the fill
    // runs instead of being skipped.
    fs::write(f.main.join("Makefile"), "all:\n\ttrue\n").unwrap();
    let without_manifest = std::process::Command::new("/bin/bash")
        .args(["-c", crate::writer::fill::CARGO_CHECK])
        .current_dir(&f.main)
        .output()
        .unwrap();
    assert!(
        without_manifest.status.success(),
        "{}",
        String::from_utf8_lossy(&without_manifest.stderr)
    );
    assert!(
        without_manifest.stderr.is_empty(),
        "a no-Cargo tree ran a Cargo check: {}",
        String::from_utf8_lossy(&without_manifest.stderr)
    );
    // A Cargo tree hands control to the preflight, which runs the offline
    // metadata check. The sibling is a real package beside the repository, so
    // the only reason to refuse the tree is that it lives outside.
    fs::create_dir_all(f.sibling.join("src")).unwrap();
    fs::write(
        f.sibling.join("Cargo.toml"),
        "[package]\nname='sibling'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(f.sibling.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    commit(&f.sibling, "sibling\n");
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname='preflight'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nsibling={path='../commons'}\n",
    )
    .unwrap();
    fs::write(
        f.main.join("Cargo.lock"),
        "version=4\n[[package]]\nname='preflight'\nversion='0.1.0'\ndependencies=['sibling']\n[[package]]\nname='sibling'\nversion='0.1.0'\n",
    )
    .unwrap();
    let cargo = std::process::Command::new("python3")
        .args(["-c", crate::writer::fill::PREFLIGHT])
        .current_dir(&f.main)
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_HOME", f.scratch.join("empty-cargo"))
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_KEY_0")
        .env_remove("GIT_CONFIG_VALUE_0")
        .output()
        .unwrap();
    assert!(!cargo.status.success(), "a sibling path dependency passed");
    assert!(
        String::from_utf8_lossy(&cargo.stderr).contains("path dependencies outside the repository"),
        "{}",
        String::from_utf8_lossy(&cargo.stderr)
    );
}

/// A no-Cargo repository's declared fill is queued as a writer job and runs its
/// command, and the same commit is reused rather than filled again.
#[tokio::test]
async fn a_no_cargo_repository_runs_its_declared_fill() {
    let f = Fixture::new();
    // No root Cargo.toml anywhere: without a declaration this tree has nothing
    // to warm, and the runner-wide override is cleared so the declaration is
    // the only source of a command.
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/main.c"), "int main(void){return 0;}\n").unwrap();
    fs::write(
        f.main.join("Makefile"),
        "all:\n\tprintf declared > command-marker\n",
    )
    .unwrap();
    commit(&f.main, "c sources\n");
    commit_declaration(
        &f,
        "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"src/**\", \"Makefile\"]\n",
    );
    let mut config = f.config();
    config.base_commands.clear();
    // The runner process itself is what fills here; on a macOS host that needs
    // the host backend allowed, as the other runner tests do.
    config.development_allow_host_backend = true;
    let runner = Runner::open(config.clone()).unwrap();
    warm(&f, &runner).await;
    let writers: Vec<Job> = runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .filter(|job| job.base)
        .cloned()
        .collect();
    assert_eq!(writers.len(), 1, "the declared fill was not queued");
    let request = writers[0].request.as_ref().unwrap();
    assert_eq!(request.command, "make");
    assert_eq!(
        fs::read(Path::new(&request.cwd).join("command-marker")).unwrap(),
        b"declared",
        "the declared command did not run"
    );
    let status = &runner.status().repositories[0];
    assert_eq!(status.last_rebuild_result, Some(RebuildResult::Ok));
    assert_eq!(
        status.published_commit.clone(),
        Some(git(&f.main, &["rev-parse", "HEAD"]))
    );
    // The same declaration at a later commit reuses that fill.
    drop(runner);
    fs::create_dir_all(f.main.join("docs")).unwrap();
    fs::write(f.main.join("docs/readme.md"), "notes\n").unwrap();
    commit(&f.main, "unlisted file\n");
    let runner = reopen(config);
    warm(&f, &runner).await;
    assert_eq!(
        runner
            .core
            .inner
            .lock()
            .unwrap()
            .jobs
            .values()
            .filter(|job| job.base)
            .count(),
        1,
        "an unlisted file queued a second fill"
    );
}

/// Without a declaration a Cargo tree must get exactly the fill it gets today:
/// Cargo's own warm command, and the same shortcut rules.
#[test]
fn a_cargo_tree_without_a_declaration_keeps_todays_fill() {
    let f = Fixture::new();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    let plain = declare(&f, "[workspace]\nmembers = []\n");
    assert_eq!(
        warm_steps(&f.main, &plain),
        (
            vec!["cargo build --workspace --all-targets".to_string()],
            false
        ),
        "an undeclared Cargo tree warms with Cargo's default alone"
    );
    // No declaration is read for this tip, so the descriptor has exactly the
    // inputs it always had.
    assert_eq!(
        declaration::read(&f.main, &plain).unwrap(),
        None,
        "the fixture declared something"
    );
    let (steps, _) = warm_steps(&f.main, &plain);
    let command = steps.join(" && ");
    assert_eq!(
        command,
        workspace::manifest_warm_command("[workspace]\nmembers = []\n", "tip"),
        "the command changed for an undeclared Cargo tree"
    );
    let with_none = descriptor(
        &f.main,
        &plain,
        "rustc A",
        &command,
        None,
        &Compilers::default(),
    )
    .unwrap();
    assert_eq!(with_none.len(), 64, "the descriptor changed shape");
    // The shortcut still applies to a graph with nothing to cache.
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname='plain'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(
        f.main.join("Cargo.lock"),
        "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"plain\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let package = commit(&f.main, "package\n").0;
    assert_eq!(
        warm_steps(&f.main, &package),
        (
            vec!["cargo build --workspace --all-targets".to_string()],
            false
        )
    );
    assert!(dependency_free(&f.main, &package, None).unwrap());
}

/// A Cargo tree that also declares ccache steps runs Cargo's warm command and
/// then each declared step, in order.
#[test]
fn a_cargo_tree_runs_its_declared_steps_after_the_cargo_warm_command() {
    let f = Fixture::new();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    fs::write(
        f.main.join("Cargo.toml"),
        "[workspace]\nmembers = []\n[workspace.metadata.motor]\nwarm-commands = [\"cargo build --workspace\"]\n",
    )
    .unwrap();
    git(&f.main, &["add", "."]);
    let tip = commit_declaration(
        &f,
        "[[fill]]\ntool = \"ccache\"\ncommands = [\"make deps\"]\n\"descriptor-inputs\" = [\"deps/**\"]\n\n[[fill]]\ntool = \"ccache\"\ncommands = [\"make\", \"make check\"]\n\"descriptor-inputs\" = [\"src/**\"]\n",
    );
    let (steps, explicit) = warm_steps(&f.main, &tip);
    assert_eq!(
        steps,
        vec![
            "cargo build --workspace".to_string(),
            "make deps".to_string(),
            "make && make check".to_string(),
        ],
        "the Cargo command and the declared steps did not run in order"
    );
    assert!(explicit, "a fill with declared steps must run");
    // The descriptor covers both halves, so a declared input moves it.
    let command = steps.join(" && ");
    let compilers = Compilers::default();
    let before = descriptor(
        &f.main,
        &tip,
        "rustc A",
        &command,
        declaration::read(&f.main, &tip).unwrap().as_ref(),
        &compilers,
    )
    .unwrap();
    fs::create_dir_all(f.main.join("deps")).unwrap();
    fs::write(f.main.join("deps/lib.c"), "int value(void){return 0;}\n").unwrap();
    let after = commit(&f.main, "declared input\n").0;
    let moved = descriptor(
        &f.main,
        &after,
        "rustc A",
        &command,
        declaration::read(&f.main, &after).unwrap().as_ref(),
        &compilers,
    )
    .unwrap();
    assert_ne!(
        before, moved,
        "a declared input of a Cargo tree did not move the descriptor"
    );
}

/// A tree with neither a root `Cargo.toml` nor a declaration still warms
/// nothing: its plan is `true` and no fill needs a writer.
#[test]
fn a_tree_without_cargo_or_a_declaration_warms_nothing() {
    let f = Fixture::new();
    let tip = git(&f.main, &["rev-parse", "HEAD"]);
    assert_eq!(warm_steps(&f.main, &tip), (vec!["true".to_string()], false));
    assert_eq!(workspace::warm_plan(&f.main, &tip), ("true".into(), false));
    assert!(dependency_free(&f.main, &tip, None).unwrap());
}

/// A Cargo tree that declares a ccache step runs Cargo's warm command and then
/// the declared step in one fill, in that order.
#[tokio::test]
async fn a_cargo_fill_runs_its_declared_ccache_step_after_cargo() {
    let f = Fixture::new();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname='both'\nversion='0.1.0'\nedition='2021'\n[package.metadata.motor]\nwarm-commands = [\"printf cargo >> order\"]\n",
    )
    .unwrap();
    fs::write(
        f.main.join("Cargo.lock"),
        "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"both\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    git(&f.main, &["add", "."]);
    commit_declaration(
        &f,
        "[[fill]]\ntool = \"ccache\"\ncommands = [\"printf ccache >> order\", \"printf done >> order\"]\n\"descriptor-inputs\" = [\"src/**\"]\n",
    );
    let mut config = f.config();
    config.base_commands.clear();
    config.development_allow_host_backend = true;
    let runner = Runner::open(config).unwrap();
    warm(&f, &runner).await;
    let writers: Vec<Job> = runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .values()
        .filter(|job| job.base)
        .cloned()
        .collect();
    assert_eq!(writers.len(), 1, "one fill, not one per step");
    let request = writers[0].request.as_ref().unwrap();
    assert_eq!(
        request.command,
        "printf cargo >> order && printf ccache >> order && printf done >> order"
    );
    assert_eq!(
        fs::read_to_string(Path::new(&request.cwd).join("order")).unwrap(),
        "cargoccachedone",
        "the Cargo command and then each declared step did not run in order"
    );
    assert_eq!(
        runner.status().repositories[0].last_rebuild_result,
        Some(RebuildResult::Ok)
    );
}

/// A declared step that fails fails the whole fill, so a fill that stopped
/// early is never recorded as filled.
#[tokio::test]
async fn a_failing_declared_step_fails_the_fill() {
    let f = Fixture::new();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
    fs::write(
        f.main.join("Cargo.toml"),
        "[package]\nname='fails'\nversion='0.1.0'\nedition='2021'\n[package.metadata.motor]\nwarm-commands = [\"true\"]\n",
    )
    .unwrap();
    fs::write(
        f.main.join("Cargo.lock"),
        "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"fails\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    git(&f.main, &["add", "."]);
    commit_declaration(
        &f,
        "[[fill]]\ntool = \"ccache\"\ncommands = [\"exit 3\", \"printf never\"]\n\"descriptor-inputs\" = [\"src/**\"]\n",
    );
    let mut config = f.config();
    config.base_commands.clear();
    config.development_allow_host_backend = true;
    let runner = Runner::open(config).unwrap();
    warm(&f, &runner).await;
    let repository = &runner.status().repositories[0];
    assert_eq!(
        repository.last_rebuild_result,
        Some(RebuildResult::Failed),
        "a failed declared step left the fill successful"
    );
    assert_eq!(repository.published_commit, None);
}
