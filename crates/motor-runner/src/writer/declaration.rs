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
    /// no step is declared. The fill runs the steps as one shell command, so a
    /// failing step stops the ones after it.
    #[cfg(test)]
    pub(crate) fn command(&self) -> Option<String> {
        let commands: Vec<String> = self.fills.iter().map(Step::command).collect();
        (!commands.is_empty()).then(|| commands.join(" && "))
    }

    /// The tool of every declared step, once each, in declaration order.
    #[cfg(test)]
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
/// Every refusal names what to change. A file the runner cannot read is refused
/// rather than ignored: dropping a declaration would leave the repository warm
/// from something other than what it declared.
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

/// Refuse a pattern that is empty, absolute, names a parent directory, or can
/// select no file at all.
///
/// "Selects no file" is answered by the matcher, using a sample path the
/// pattern itself names, rather than by a second reading of the wildcard syntax
/// here: what the parser accepts and what a pattern matches cannot then drift
/// apart. Only a `..` segment is refused outright, because that is the one shape
/// that reaches outside the repository it is matched against.
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
    // A sample path the pattern itself names, so a pattern that names no file
    // at all is refused rather than accepted. Literal segments are used as
    // written. A wildcard segment becomes an ordinary name, ending in the file
    // extension the segment states when it states one (so `*.h` samples
    // `plain.h`); a range becomes its first value, and `**` one directory.
    let mut path: Vec<String> = Vec::new();
    for segment in pattern.split('/') {
        if segment == "**" {
            path.push("plain".to_string());
            continue;
        }
        if let Some((start, end)) = segment.split_once("..") {
            if !end.contains("..") {
                if let (Ok(start), Ok(end)) = (start.parse::<i64>(), end.parse::<i64>()) {
                    path.push(start.min(end).to_string());
                    continue;
                }
            }
        }
        let literal: String = segment
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
            .collect();
        let extension = segment
            .rsplit_once('.')
            .map(|(_, extension)| extension)
            .filter(|extension| !extension.is_empty())
            .filter(|extension| {
                extension
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            });
        path.push(match extension {
            Some(extension) if literal.starts_with('.') => format!("plain.{extension}"),
            _ if !literal.is_empty() && !literal.starts_with('.') => literal,
            _ => "plain".to_string(),
        });
    }
    let sample = [path.join("/")];
    if !sample.iter().any(|name| path_matches(pattern, name)) {
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

/// The files of `commit`'s tree a declared fill depends on.
///
/// A pattern is a repository-relative path with shell wildcards, and it matches
/// whole paths, one segment at a time:
///
/// - `*` matches any run of characters inside one name, `?` one character, and
///   `[abc]`, `[a-z]` and `[!abc]` one character from a set;
/// - a segment that is exactly `**` stands for any number of directories;
/// - `**` at the end of a pattern names everything below its directory;
/// - a pattern with a `/` names a path from the repository root, and one
///   without names a file in the root only;
/// - a name starting with `.` is reached only by a segment that also starts
///   with `.`, and `**` on its own names no hidden name;
/// - a trailing `/` names a directory, which no tracked file is.
///
/// These are the rules Python's glob applies to a tracked file list, which is
/// what this replaces, minus two quirks that came from matching a list of
/// strings rather than paths: a `*` crossing `/`, and `**/x` failing on a
/// top-level `x`. A declaration is easier to trust when a pattern means what its
/// path spelling says.
///
/// The matched paths come back sorted and without duplicates, so the order the
/// tree lists them in cannot leak into the hash. Nothing here can climb out of
/// the repository: a matched name never becomes a path, and a tree holds no
/// `..` entry, so every file the hash reads came from the commit itself.
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

/// Whether the declared `pattern` names the file at `path`.
pub(crate) fn path_matches(pattern: &str, path: &str) -> bool {
    // A directory pattern names no file.
    if pattern.ends_with('/') {
        return false;
    }
    let segments: Vec<&str> = pattern.split('/').collect();
    let names: Vec<&str> = path.split('/').collect();
    // A bare pattern names a file in the root, never one found by searching.
    if !pattern.contains('/') {
        return names.len() == 1 && segment_matches(segments[0], names[0]);
    }
    if !walk(&segments, &names) {
        return false;
    }
    // A hidden name is reached only by the pattern segment that names it; `**`
    // names no name of its own, so it sees none of them.
    let mut segment = 0;
    for name in &names {
        while segment < segments.len() && segments[segment] == "**" {
            segment += 1;
        }
        if hidden(name) && !segments.get(segment).is_some_and(|s| s.starts_with('.')) {
            return false;
        }
        segment += 1;
    }
    true
}

/// A name beginning with `.` is hidden from a pattern segment that does not
/// name one; `.` and `..` are not hidden names but paths, and no tree holds
/// them.
fn hidden(name: &str) -> bool {
    name.starts_with('.') && name.len() > 1
}

/// Match pattern segments against path names, in order.
fn walk(segments: &[&str], names: &[&str]) -> bool {
    let Some((segment, rest)) = segments.split_first() else {
        return names.is_empty();
    };
    if *segment == "**" {
        // At the end, `**` names everything below: at least one file.
        if rest.is_empty() {
            return !names.is_empty();
        }
        // Otherwise it stands for the directories between: one or more, so
        // `src/**/*.c` means the files below `src`, not the ones directly in it.
        return (1..=names.len()).any(|skip| walk(rest, &names[skip..]));
    }
    let Some((name, rest_names)) = names.split_first() else {
        return false;
    };
    segment_matches(segment, name) && walk(rest, rest_names)
}

/// One pattern segment against one name: comma-split alternatives, each a shell
/// wildcard pattern, or a numeric range written `3..9`.
fn segment_matches(segment: &str, name: &str) -> bool {
    segment
        .split(',')
        .any(|choice| match choice.split_once("..") {
            Some((start, end)) if !end.contains("..") => {
                matches!(
                    (start.parse::<i64>(), end.parse::<i64>(), name.parse::<i64>()),
                    (Ok(start), Ok(end), Ok(value))
                        if (start.min(end)..=start.max(end)).contains(&value)
                )
            }
            _ => wildcard(choice, name),
        })
}

/// `*`, `?`, `[abc]`, `[a-z]` and `[!abc]` over a whole name.
fn wildcard(pattern: &str, name: &str) -> bool {
    wildcard_from(pattern, name)
}

fn wildcard_from(pattern: &str, name: &str) -> bool {
    let Some(first) = pattern.chars().next() else {
        return name.is_empty();
    };
    let rest = &pattern[first.len_utf8()..];
    match first {
        '*' => (0..=name.len())
            .filter(|skip| name.is_char_boundary(*skip))
            .any(|skip| wildcard_from(rest, &name[skip..])),
        '?' => match name.chars().next() {
            Some(character) => wildcard_from(rest, &name[character.len_utf8()..]),
            None => false,
        },
        '[' => {
            let (set, rest) = match character_set(rest) {
                Some(set) => set,
                // An unclosed bracket is a literal one.
                None => return name.starts_with('[') && wildcard_from(rest, &name[1..]),
            };
            match name.chars().next() {
                Some(character) => {
                    set_matches(set, character)
                        && wildcard_from(rest, &name[character.len_utf8()..])
                }
                None => false,
            }
        }
        literal => match name.chars().next() {
            Some(character) => {
                character == literal && wildcard_from(rest, &name[character.len_utf8()..])
            }
            None => false,
        },
    }
}

/// A `[...]` set at the start of `rest`, and what follows its closing `]`.
fn character_set(rest: &str) -> Option<(&str, &str)> {
    let negated = rest.starts_with('!');
    let body = if negated { &rest[1..] } else { rest };
    // A `]` that opens the set is a member, not the end.
    let search_from = usize::from(body.starts_with(']'));
    let close = body[search_from..].find(']')? + search_from;
    Some((&rest[..close + usize::from(negated)], &body[close + 1..]))
}

fn set_matches(set: &str, character: char) -> bool {
    let negated = set.starts_with('!');
    let body = if negated { &set[1..] } else { set };
    let characters: Vec<char> = body.chars().collect();
    let mut matched = false;
    let mut index = 0;
    while index < characters.len() {
        if index + 2 < characters.len() && characters[index + 1] == '-' {
            if (characters[index]..=characters[index + 2]).contains(&character) {
                matched = true;
            }
            index += 3;
        } else {
            if characters[index] == character {
                matched = true;
            }
            index += 1;
        }
    }
    matched != negated
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

#[cfg(test)]
#[path = "declaration_tests.rs"]
mod tests;
