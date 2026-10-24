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

use crate::writer::declaration::{self, Compilers, Declaration, Step, Tool};
use crate::writer::fill::{dependency_free, descriptor};

#[test]
fn a_descriptor_changes_with_its_declared_steps_and_matched_files() {
    let f = Fixture::new();
    fs::write(f.main.join("Makefile"), "all:\n\tcc src/main.c\n").unwrap();
    fs::create_dir_all(f.main.join("src")).unwrap();
    fs::write(f.main.join("src/main.c"), "int main(void){return 0;}\n").unwrap();
    let declare = |stanza: &str| commit_declaration(&f, stanza);
    let base = format!(
        "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"src/**\", \"Makefile\"]\n"
    );
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
    let first_tip = declare(&base);
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
