//! Fill declarations from the repository's `.motor/cache.toml`.
//!
//! A tree without a root `Cargo.toml` has no warm command of its own: the
//! runner cannot derive one, so a repository that builds outside Cargo says
//! what to run, and which of its files that run depends on, in this file.
//!
//! ```toml
//! [[fill]]
//! tool = "ccache"
//! commands = ["make"]
//! descriptor-inputs = ["src/**", "Makefile"]
//! ```
//!
//! Each `[[fill]]` table is one fill step. The schema is deliberately small and
//! strict: unknown keys, an unknown tool, and a pattern escaping the repository
//! are refused, because a descriptor is only trustworthy if every part of it is
//! understood. A refusal fails the tip, the same way an unavailable tip fails.
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use crate::workspace;

/// The declaration file, relative to the repository root.
pub(crate) const PATH: &str = ".motor/cache.toml";

/// The tools a fill can declare, by the id a descriptor hashes.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Tool {
    Sccache,
    Ccache,
}

impl Tool {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Tool::Sccache => "sccache",
            Tool::Ccache => "ccache",
        }
    }

    fn parse(id: &str) -> Option<Self> {
        match id {
            "sccache" => Some(Tool::Sccache),
            "ccache" => Some(Tool::Ccache),
            _ => None,
        }
    }
}

/// One `[[fill]]` table, exactly as the repository wrote it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Step {
    pub(crate) tool: Tool,
    pub(crate) commands: Vec<String>,
    pub(crate) descriptor_inputs: Vec<String>,
}

impl Step {
    /// The commands as the single shell command that runs them.
    pub(crate) fn command(&self) -> String {
        self.commands.join(" && ")
    }
}

/// The whole file: its declared steps, in declaration order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Declaration {
    pub(crate) fills: Vec<Step>,
}

impl Declaration {
    /// One command for the shell that runs the steps in order, or `None` when
    /// no step is declared.
    pub(crate) fn command(&self) -> Option<String> {
        let commands: Vec<String> = self.fills.iter().map(Step::command).collect();
        (!commands.is_empty()).then(|| commands.join(" && "))
    }

    /// The tool of every declared step, once each, in declaration order.
    pub(crate) fn tools(&self) -> Vec<Tool> {
        let mut tools = Vec::new();
        for step in &self.fills {
            if !tools.contains(&step.tool) {
                tools.push(step.tool);
            }
        }
        tools
    }
}

/// The declaration `commit` carries, or `None` when its tree has none.
pub(crate) fn read(mirror: &Path, commit: &str) -> io::Result<Option<Declaration>> {
    let tree = workspace::git(mirror, &["ls-tree", "-r", "--name-only", "-z", commit])?;
    if !tree.split('\0').any(|path| path == PATH) {
        return Ok(None);
    }
    let text = workspace::git(mirror, &["show", &format!("{commit}:{PATH}")])?;
    parse(&text).map(Some).map_err(crate::storage::invalid)
}

/// Parse `.motor/cache.toml`.
///
/// A Cargo tree already owns its fill through `[workspace.metadata.motor]`, so
/// a table it cannot have meant is refused with what to change; a tree the
/// runner has no warm command for is refused for the same reason: silently
/// dropping a declaration would leave the repository warm from something other
/// than what it declared.
pub(crate) fn parse(text: &str) -> Result<Declaration, String> {
    let document: toml::Table = toml::from_str(text).map_err(|error| error.to_string())?;
    for key in document.keys() {
        if key != "fill" {
            return Err(format!(
                "unknown key {key:?}; only [[fill]] tables are read"
            ));
        }
    }
    let Some(fills) = document.get("fill") else {
        return Err("no [[fill]] table is declared".into());
    };
    let Some(fills) = fills.as_array() else {
        return Err("fill must be an array of [[fill]] tables".into());
    };
    if fills.is_empty() {
        return Err("no [[fill]] table is declared".into());
    }
    let mut steps = Vec::new();
    for (index, fill) in fills.iter().enumerate() {
        let label = format!("[[fill]] #{}", index + 1);
        let Some(table) = fill.as_table() else {
            return Err(format!("{label} is not a table"));
        };
        for key in table.keys() {
            if !matches!(key.as_str(), "tool" | "commands" | "descriptor-inputs") {
                return Err(format!("{label}: unknown key {key:?}"));
            }
        }
        let tool = table
            .get("tool")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| format!("{label}: tool must be one of \"sccache\" or \"ccache\""))?;
        let tool = Tool::parse(tool)
            .ok_or_else(|| format!("{label}: unknown tool {tool:?}; expected sccache or ccache"))?;
        let commands = strings(table, "commands", &label)?;
        let descriptor_inputs = strings(table, "descriptor-inputs", &label)?;
        for pattern in &descriptor_inputs {
            check_pattern(pattern).map_err(|error| format!("{label}: {error}"))?;
        }
        steps.push(Step {
            tool,
            commands,
            descriptor_inputs,
        });
    }
    Ok(Declaration { fills: steps })
}

/// Refuse a pattern that is absolute, always matches nothing, or can step
/// above the root it is matched against.
///
/// The last three cases are answered by the matcher itself rather than by a
/// second reading of the syntax here, so what the parser accepts and what a
/// pattern actually matches cannot drift apart. The `.` and `..` names are
/// named patterns rather than literals: `.` describes every name, and `..`
/// describes a parent, which no tree entry is.
fn check_pattern(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("empty descriptor-inputs pattern".into());
    }
    if pattern.starts_with('/') {
        return Err(format!(
            "descriptor-inputs pattern is absolute, and patterns match repository-relative paths: {pattern}"
        ));
    }
    for segment in pattern.split('/') {
        if segment == ".." {
            return Err(format!(
                "descriptor-inputs pattern steps above the root: {pattern}"
            ));
        }
    }
    const ANY_NAME: [&str; 4] = ["a", "a/b", ".a", "a/.b"];
    if !ANY_NAME.iter().any(|name| path_matches(pattern, name)) {
        return Err(format!(
            "descriptor-inputs pattern matches nothing: {pattern}"
        ));
    }
    Ok(())
}

fn strings(table: &toml::Table, key: &str, label: &str) -> Result<Vec<String>, String> {
    let values = table
        .get(key)
        .and_then(toml::Value::as_array)
        .ok_or_else(|| format!("{label}: {key} must be a non-empty list of strings"))?;
    if values.is_empty() {
        return Err(format!("{label}: {key} must not be empty"));
    }
    values
        .iter()
        .map(|value| match value.as_str() {
            // A blank command or pattern can only have been a mistake, and an
            // empty pattern would silently match nothing.
            Some(text) if !text.trim().is_empty() => Ok(text.to_string()),
            _ => Err(format!("{label}: {key} must hold non-empty strings")),
        })
        .collect()
}

/// One declared fill's contribution to a descriptor: its tool id, then its
/// commands, then its `descriptor-inputs` patterns, each list closed by an
/// empty field so no two different declarations can hash the same bytes.
pub(crate) fn step_hash(hash: &mut sha2::Sha256, step: &Step) {
    use sha2::Digest;
    fn field(hash: &mut sha2::Sha256, text: &str) {
        hash.update(text.as_bytes());
        hash.update([0]);
    }
    field(hash, step.tool.id());
    for command in &step.commands {
        field(hash, command);
    }
    field(hash, "");
    for pattern in &step.descriptor_inputs {
        field(hash, pattern);
    }
    field(hash, "");
}

/// The files of `commit`'s tree the declaration depends on.
///
/// Patterns follow Python's glob, because this rule replaces a check written
/// in Python and must accept exactly the same files: `*`, `?`, `[abc]` and
/// `[a-z]` within one path segment, `**` across segments, and the leading-dot
/// rule (a name starting with `.` is matched only by a segment that itself
/// starts with `.`). A trailing `/` adds nothing to a list of files. The
/// matched paths come back sorted and without duplicates, so the order the
/// tree lists them in cannot leak into the hash.
///
/// None of that syntax can climb out of the repository: a matched name never
/// becomes a path, and the tree has no `..` entry, so every file the hash
/// reads came from the commit itself.
pub(crate) fn matched<'a>(tree: &[&'a str], inputs: &[String]) -> Vec<&'a str> {
    let mut matched: Vec<&str> = tree
        .iter()
        .copied()
        .filter(|path| inputs.iter().any(|pattern| path_matches(pattern, path)))
        .collect();
    matched.sort_unstable();
    matched.dedup();
    matched
}

/// Whether `path` matches `pattern` under the rules in [`matched`].
pub(crate) fn path_matches(pattern: &str, path: &str) -> bool {
    let segments: Vec<&str> = pattern.split('/').collect();
    let segments = trim_empty_tail(&segments);
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    let path: Vec<&str> = path.split('/').collect();
    match_segments(&segments, &path, 0)
}

/// A name beginning with `.` is only visible to a pattern segment that also
/// names one, so a leading `**` cannot step into it.
fn hidden(segment: &str) -> bool {
    segment.starts_with('.')
}

fn match_segments(segments: &[&str], path: &[&str], from: usize) -> bool {
    let Some((segment, rest)) = segments.split_first() else {
        return from == path.len();
    };
    // `**` stands for this directory and any number of directories below it.
    if *segment == "**" {
        if match_segments(rest, path, from) {
            return true;
        }
        if from >= path.len() || rest.is_empty() || hidden(path[from]) {
            return false;
        }
        return match_segments(segments, path, from + 1);
    }
    let Some(name) = path.get(from) else {
        return false;
    };
    if hidden(name) && !hidden(segment) {
        return false;
    }
    volume_matches(segment, name) && match_segments(rest, path, from + 1)
}

fn trim_empty_tail(segments: &[&str]) -> Vec<String> {
    let mut end = segments.len();
    while end > 1 && segments[end - 1].is_empty() {
        end -= 1;
    }
    segments[..end].iter().map(|s| (*s).to_string()).collect()
}

/// One path segment: a comma-separated list of dot-split alternatives, each
/// optionally a numeric range. This is the syntax Python's glob inherited from
/// shell path expansion.
fn volume_matches(segment: &str, name: &str) -> bool {
    for choice in segment.split(',') {
        let mut parts = choice.split("..");
        let (start, end) = match (parts.next(), parts.next(), parts.next()) {
            (Some(start), None, None) => (start, None),
            (Some(start), Some(end), None) => (start, Some(end)),
            _ => continue,
        };
        if let Some(end) = end {
            let crossed = match (
                start.parse::<i64>(),
                end.parse::<i64>(),
                name.parse::<i64>(),
            ) {
                (Ok(start), Ok(end), Ok(value)) if start <= end => (start..=end).contains(&value),
                (Ok(start), Ok(end), Ok(value)) => (end..=start).contains(&value),
                _ => false,
            };
            if crossed {
                return true;
            }
            continue;
        }
        let parts: Vec<&str> = choice.split('.').collect();
        // A lone number names that one value; anything else compares the
        // dot-separated words of the name, each as a shell pattern.
        if parts.len() == 1 {
            if name == start {
                return true;
            }
            continue;
        }
        if parts.len() == name.split('.').count()
            && parts
                .iter()
                .zip(name.split('.'))
                .all(|(part, word)| wildcard(part, word))
        {
            return true;
        }
    }
    false
}

/// `*`, `?`, `[seq]` and `[!seq]` inside one dot-separated word.
fn wildcard(pattern: &str, word: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let word: Vec<char> = word.chars().collect();
    wildcard_from(&pattern, &word)
}

fn wildcard_from(pattern: &[char], word: &[char]) -> bool {
    let Some((&first, rest)) = pattern.split_first() else {
        return word.is_empty();
    };
    match first {
        '*' => (0..=word.len()).any(|skip| wildcard_from(rest, &word[skip..])),
        '?' => !word.is_empty() && wildcard_from(rest, &word[1..]),
        '[' => {
            let Some(close) = closing_bracket(rest) else {
                // An unclosed bracket is a literal one, as in fnmatch.
                return word.first() == Some(&'[') && wildcard_from(rest, &word[1..]);
            };
            let Some(character) = word.first() else {
                return false;
            };
            set_matches(&rest[..close], *character) && wildcard_from(&rest[close + 1..], &word[1..])
        }
        literal => word.first() == Some(&literal) && wildcard_from(rest, &word[1..]),
    }
}

/// The index of the `]` closing a character set, or `None` when it never
/// closes; a `]` that opens the set is a literal member.
fn closing_bracket(rest: &[char]) -> Option<usize> {
    let start = usize::from(matches!(rest.first(), Some('!')));
    rest.iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, character)| **character == ']')
        .map(|(index, _)| index)
}

fn set_matches(set: &[char], character: char) -> bool {
    let negate = matches!(set.first(), Some('!'));
    let set = if negate { &set[1..] } else { set };
    let mut matched = false;
    let mut index = 0;
    while index < set.len() {
        if index + 2 < set.len() && set[index + 1] == '-' {
            if (set[index]..=set[index + 2]).contains(&character) {
                matched = true;
            }
            index += 3;
        } else {
            if set[index] == character {
                matched = true;
            }
            index += 1;
        }
    }
    matched != negate
}

/// The toolchain a declared fill's descriptor extends: the build server's
/// C/C++ compiler and ccache versions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Compilers {
    pub(crate) versions: BTreeMap<String, String>,
}

impl Compilers {
    /// `program --version`, cut at its first line, for each program. A program
    /// the build server does not have contributes nothing, so its descriptor
    /// cannot depend on a version nobody read.
    pub(crate) fn probe(programs: &[&str]) -> Self {
        let mut versions = BTreeMap::new();
        for program in programs {
            let Ok(output) = std::process::Command::new(program)
                .arg("--version")
                .output()
            else {
                continue;
            };
            if !output.status.success() {
                continue;
            }
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = text.lines().next() {
                versions.insert((*program).to_string(), line.trim().to_string());
            }
        }
        Self { versions }
    }

    pub(crate) fn hash(&self, hash: &mut sha2::Sha256) {
        use sha2::Digest;
        for (program, version) in &self.versions {
            hash.update(program.as_bytes());
            hash.update([0]);
            hash.update(version.as_bytes());
            hash.update([0]);
        }
    }
}
