//! `.motor/cache.toml` parsing, pattern matching and descriptor inputs.
use super::*;

fn declaration(text: &str) -> Declaration {
    parse(text).unwrap_or_else(|error| panic!("{text} was refused: {error}"))
}

fn refusal(text: &str) -> String {
    match parse(text) {
        Ok(declaration) => panic!("{text} parsed as {declaration:?}"),
        Err(error) => error,
    }
}

/// One `[[fill]]` table for a tool whose single command lists one pattern.
fn stanza(tool: &str, command: &str, pattern: &str) -> String {
    format!(
        "[[fill]]\ntool = \"{tool}\"\ncommands = [\"{command}\"]\n\"descriptor-inputs\" = [\"{pattern}\"]\n"
    )
}

fn ccache() -> String {
    stanza("ccache", "make", "src/**")
}

fn sccache() -> String {
    stanza("sccache", "cargo build", "Cargo.toml")
}

#[test]
fn a_fill_table_parses_into_its_steps_in_declaration_order() {
    let text = format!(
        "{}\n[[fill]]\ncommands = [\"make clean\", \"make -j2\"]\ntool = \"ccache\"\n\"descriptor-inputs\" = [\"src/**\", \"Makefile\", \"include/*.h\"]\n",
        ccache()
    );
    let parsed = declaration(&text);
    assert_eq!(
        parsed.fills,
        vec![
            Step {
                tool: Tool::Ccache,
                commands: vec!["make".into()],
                descriptor_inputs: vec!["src/**".into()],
            },
            Step {
                tool: Tool::Ccache,
                commands: vec!["make clean".into(), "make -j2".into()],
                descriptor_inputs: vec!["src/**".into(), "Makefile".into(), "include/*.h".into()],
            },
        ]
    );
    assert_eq!(
        parsed.command().as_deref(),
        Some("make && make clean && make -j2")
    );
    assert_eq!(parsed.tools(), vec![Tool::Ccache]);
}

#[test]
fn declared_commands_are_joined_in_order_and_step_boundaries_survive() {
    let text = format!(
        "{}\n{}",
        stanza("ccache", "make clean", "src/**").replace(
            "commands = [\"make clean\"]",
            "commands = [\"make clean\", \"make\"]"
        ),
        sccache(),
    );
    let parsed = declaration(&text);
    assert_eq!(
        parsed.command().as_deref(),
        Some("make clean && make && cargo build")
    );
    assert_eq!(parsed.tools(), vec![Tool::Ccache, Tool::Sccache]);
    // A repeated tool is one tool, once.
    let repeated = declaration(&format!("{}\n{}", ccache(), ccache()));
    assert_eq!(repeated.tools(), vec![Tool::Ccache]);
    assert_eq!(
        repeated.command().as_deref(),
        Some("make && make"),
        "two steps of one tool both run"
    );
}

#[test]
fn the_parser_refuses_what_the_schema_forbids() {
    for (text, expected) in [
        ("", "no [[fill]]"),
        ("# only a comment\n", "no [[fill]]"),
        ("fill = []\n", "no [[fill]]"),
        ("fill = \"make\"\n", "fill must be an array"),
        (
            "[[fill]]\ntool = \"make\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"src/**\"]\n",
            "unknown tool",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = []\n\"descriptor-inputs\" = [\"src/**\"]\n",
            "commands must not be empty",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = []\n",
            "descriptor-inputs must not be empty",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = [\"\"]\n\"descriptor-inputs\" = [\"src/**\"]\n",
            "non-empty strings",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"/etc/passwd\"]\n",
            "absolute",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"../outside\"]\n",
            "steps above the root",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"src/../../outside\"]\n",
            "steps above the root",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"src/**\"]\nextra = 1\n",
            "unknown key \"extra\"",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"src/**\"]\nenv = { CC = \"clang\" }\n",
            "unknown key \"env\"",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"src/**\"]\nwarm-commands = [\"make\"]\n",
            "unknown key \"warm-commands\"",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = \"make\"\n\"descriptor-inputs\" = [\"src/**\"]\n",
            "commands must be a non-empty list",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"src/[abc\"]\n",
            "matches nothing",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"src/**/\"]\n",
            "matches nothing",
        ),
        (
            "[[fill]]\ntool = \"ccache\"\ncommands = [\"make\"]\n\"descriptor-inputs\" = \"src/**\"\n",
            "descriptor-inputs must be a non-empty list",
        ),
        ("[[fill]]\ntool = \"ccache\"\n", "commands must be a non-empty list"),
        (
            "[[fill]]\ncommands = [\"make\"]\n\"descriptor-inputs\" = [\"src/**\"]\n",
            "tool must be one of",
        ),
    ] {
        let error = refusal(text);
        assert!(
            error.contains(expected),
            "{text:?} was refused for the wrong reason: {error}"
        );
    }
}

#[test]
fn a_pattern_steps_above_the_root_only_as_its_own_segment() {
    // `..` inside a name is a name, not a parent directory.
}

/// The matching rules, stated one path at a time. These are the rules Python's
/// glob applies to a tracked file list (minus two quirks that came from
/// matching that list as strings rather than as paths), and each line is one
/// rule a declaration relies on.
#[test]
fn patterns_match_whole_repository_relative_paths() {
    let cases: &[(&str, &[&str])] = &[
        // `**` at the end names everything below its directory.
        (
            "src/**",
            &[
                "src/main.c",
                "src/lib/a.c",
                "src/lib/b.c",
                "src/lib/deep/b.c",
            ],
        ),
        // `**` in the middle stands for any number of directories.
        (
            "src/**/*.c",
            &["src/lib/a.c", "src/lib/b.c", "src/lib/deep/b.c"],
        ),
        ("**/Makefile", &["docs/Makefile"]),
        // A wildcard stays inside one name.
        ("src/*.c", &["src/main.c"]),
        ("include/*.h", &["include/a.h"]),
        ("src/?.c", &[]),
        // A bracket set names one character from the set or range.
        ("src/lib/[ab].c", &["src/lib/a.c", "src/lib/b.c"]),
        ("src/lib/[!a]*.c", &["src/lib/b.c"]),
        // A trailing `/` names a directory, which a tracked file is not.
        ("src/**/", &[]),
        // A hidden name is reached only by a segment that starts with `.`.
        (".config", &[".config"]),
        (".**", &[".config"]),
        // A bare pattern names a file in the root, never one found by searching.
        ("*.c", &[]),
        ("Makefile", &["Makefile"]),
    ];
    let tree = [
        "Makefile",
        "docs/Makefile",
        "src/main.c",
        "src/lib/a.c",
        "src/lib/b.c",
        "src/lib/deep/b.c",
        "include/a.h",
        ".config",
        ".motor/cache.toml",
        "src/.hidden.c",
    ];
    for (pattern, expected) in cases {
        let ours: Vec<&str> = matched(&tree, &[pattern.to_string()]);
        let ours: std::collections::BTreeSet<&str> = ours.into_iter().collect();
        let expected: std::collections::BTreeSet<&str> = expected.iter().copied().collect();
        assert_eq!(ours, expected, "{pattern}");
    }
    // A hidden file is not named by a wildcard segment, and `**` alone does not
    // see into a hidden directory.
    assert!(!path_matches("src/*.c", "src/.hidden.c"));
    assert!(!path_matches("**/cache.toml", ".motor/cache.toml"));
    assert!(path_matches(".motor/**", ".motor/cache.toml"));
}

/// Within one name, a pattern uses shell wildcards and nothing else: a `*` does
/// not cross a `/`, so a declaration cannot silently select a deeper tree.
#[test]
fn a_wildcard_never_crosses_a_directory() {
    assert!(!path_matches("src/*.c", "src/lib/a.c"));
    assert!(!path_matches("src/*", "src/lib/a.c"));
    assert!(path_matches("src/**/*.c", "src/lib/a.c"));
    assert!(path_matches("src/**", "src/lib/a.c"));
    assert!(path_matches("src/**", "src/main.c"));
}
