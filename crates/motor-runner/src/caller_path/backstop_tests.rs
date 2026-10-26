use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

const ALLOWLIST: &[&str] = &[
    "windows.rs",
    "macos.rs",
    "macos/execution.rs",
    "macos/execution_tests.rs",
    "macos/fs.rs",
    "macos/lifecycle.rs",
    "macos/lifecycle_tests.rs",
    "macos/preflight.rs",
    "macos/process.rs",
    "macos/tests.rs",
    "tenant_agent.rs",
];

#[derive(Debug)]
struct Token {
    text: String,
    line: usize,
}

// Lexing rather than line matching prevents comments, strings and whitespace
// from either inventing a field access or hiding one from the production scan.
fn tokens(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut output = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < bytes.len() {
        let start = i;
        if bytes[i].is_ascii_whitespace() {
            line += usize::from(bytes[i] == b'\n');
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    line += usize::from(bytes[i] == b'\n');
                    i += 1;
                }
            }
            continue;
        }
        let raw_start = if bytes[i] == b'b' { i + 1 } else { i };
        let raw = if bytes.get(raw_start) == Some(&b'r') {
            let mut quote = raw_start + 1;
            while bytes.get(quote) == Some(&b'#') {
                quote += 1;
            }
            (bytes.get(quote) == Some(&b'"')).then_some((quote, quote - raw_start - 1))
        } else {
            None
        };
        let quoted = bytes[i] == b'"' || (bytes[i] == b'b' && bytes.get(i + 1) == Some(&b'"'));
        if let Some((quote, hashes)) = raw {
            i = quote + 1;
            while i < bytes.len() {
                if bytes[i] == b'"'
                    && bytes
                        .get(i + 1..i + 1 + hashes)
                        .is_some_and(|s| s.iter().all(|b| *b == b'#'))
                {
                    i += hashes + 1;
                    break;
                }
                i += 1;
            }
            let text = format!(
                "literal:{}",
                &source[quote + 1..i.saturating_sub(hashes + 1)]
            );
            output.push(Token { text, line });
            line += source[start..i].bytes().filter(|b| *b == b'\n').count();
            continue;
        }
        if quoted {
            if bytes[i] == b'b' {
                i += 1;
            }
            let quote = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            output.push(Token {
                text: format!("literal:{}", &source[quote + 1..i.saturating_sub(1)]),
                line,
            });
            line += source[start..i].bytes().filter(|b| *b == b'\n').count();
            continue;
        }
        if bytes[i] == b'\''
            && (bytes.get(i + 2) == Some(&b'\'')
                || (bytes.get(i + 1) == Some(&b'\\') && bytes.get(i + 3) == Some(&b'\'')))
        {
            i += if bytes[i + 1] == b'\\' { 4 } else { 3 };
            continue;
        }
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
        } else {
            i += 1;
            while !source.is_char_boundary(i) {
                i += 1;
            }
        }
        output.push(Token {
            text: source[start..i].into(),
            line,
        });
    }
    output
}

fn close(tokens: &[Token], start: usize, open: &str, shut: &str) -> usize {
    let mut depth = 0;
    for (i, token) in tokens.iter().enumerate().skip(start) {
        if token.text == open {
            depth += 1;
        }
        if token.text == shut {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }
    panic!("unbalanced Rust item at token {start}");
}

fn requires_test(attr: &[&str]) -> bool {
    if attr == ["cfg", "(", "test", ")"] {
        return true;
    }
    if !attr.starts_with(&["cfg", "(", "all", "("]) {
        return false;
    }
    let mut depth = 0;
    for token in &attr[4..] {
        match *token {
            "test" if depth == 0 => return true,
            "(" => depth += 1,
            ")" if depth == 0 => break,
            ")" => depth -= 1,
            _ => (),
        }
    }
    false
}

fn normalized_module_path(path: PathBuf) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => (),
            std::path::Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn module_directory(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap();
    if matches!(
        path.file_stem().and_then(|s| s.to_str()),
        Some("lib" | "main" | "mod")
    ) {
        parent.to_path_buf()
    } else {
        parent.join(path.file_stem().unwrap())
    }
}

fn module_start(input: &[Token], mut index: usize) -> Option<usize> {
    if input.get(index).is_some_and(|t| t.text == "pub") {
        index += 1;
        if input.get(index).is_some_and(|t| t.text == "(") {
            index = close(input, index, "(", ")") + 1;
        }
    }
    input
        .get(index)
        .is_some_and(|t| t.text == "mod")
        .then_some(index)
}

fn module_file(
    directory: &Path,
    attribute_directory: &Path,
    name: &str,
    explicit_path: Option<&str>,
) -> PathBuf {
    let path = if let Some(explicit) = explicit_path {
        attribute_directory.join(explicit)
    } else {
        let file = directory.join(format!("{name}.rs"));
        if file.exists() {
            file
        } else {
            directory.join(name).join("mod.rs")
        }
    };
    normalized_module_path(path)
}

fn module_files(input: &[Token], directory: &Path, attribute_directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut i = 0;
    while i < input.len() {
        let first = i;
        let mut explicit_path = None;
        while input.get(i).is_some_and(|t| t.text == "#")
            && input.get(i + 1).is_some_and(|t| t.text == "[")
        {
            let end = close(input, i + 1, "[", "]");
            let attr: Vec<_> = input[i + 2..end].iter().map(|t| t.text.as_str()).collect();
            if attr.starts_with(&["path", "="]) {
                explicit_path = attr
                    .get(2)
                    .and_then(|s| s.strip_prefix("literal:"))
                    .map(String::from);
            }
            i = end + 1;
        }
        if let Some(item) = module_start(input, i) {
            let name = &input[item + 1].text;
            match input.get(item + 2).map(|t| t.text.as_str()) {
                Some(";") => {
                    files.push(module_file(
                        directory,
                        attribute_directory,
                        name,
                        explicit_path.as_deref(),
                    ));
                    i = item + 3;
                    continue;
                }
                Some("{") => {
                    let end = close(input, item + 2, "{", "}");
                    let nested = directory.join(explicit_path.as_deref().unwrap_or(name));
                    files.extend(module_files(&input[item + 3..end], &nested, &nested));
                    i = end + 1;
                    continue;
                }
                _ => (),
            }
        }
        i = i.max(first + 1);
    }
    files
}

fn production_tokens(source: &str, path: &Path) -> (Vec<Token>, Vec<PathBuf>, Vec<PathBuf>) {
    let input = tokens(source);
    let directory = module_directory(path);
    let children = module_files(&input, &directory, path.parent().unwrap());
    let mut output = Vec::new();
    let mut test_files = Vec::new();
    let mut i = 0;
    while i < input.len() {
        if input[i].text != "#" || input.get(i + 1).is_none_or(|t| t.text != "[") {
            output.push(Token {
                text: input[i].text.clone(),
                line: input[i].line,
            });
            i += 1;
            continue;
        }
        let first = i;
        let mut test = false;
        let mut explicit_path = None;
        while input.get(i).is_some_and(|t| t.text == "#")
            && input.get(i + 1).is_some_and(|t| t.text == "[")
        {
            let end = close(&input, i + 1, "[", "]");
            let attr: Vec<_> = input[i + 2..end].iter().map(|t| t.text.as_str()).collect();
            test |= attr == ["test"] || attr == ["tokio", ":", ":", "test"] || requires_test(&attr);
            if attr.starts_with(&["path", "="]) {
                explicit_path = attr
                    .get(2)
                    .and_then(|s| s.strip_prefix("literal:"))
                    .map(String::from);
            }
            i = end + 1;
        }
        if !test {
            for token in &input[first..i] {
                output.push(Token {
                    text: token.text.clone(),
                    line: token.line,
                });
            }
            continue;
        }
        let end = (i..input.len())
            .find(|j| matches!(input[*j].text.as_str(), ";" | "{"))
            .expect("test item body");
        if input[end].text == ";" {
            if let Some(item) = module_start(&input, i) {
                test_files.push(module_file(
                    &directory,
                    path.parent().unwrap(),
                    &input[item + 1].text,
                    explicit_path.as_deref(),
                ));
            }
            i = end + 1;
        } else {
            let body_end = close(&input, end, "{", "}");
            let nested = module_start(&input, i).map_or_else(
                || directory.clone(),
                |item| directory.join(explicit_path.as_deref().unwrap_or(&input[item + 1].text)),
            );
            test_files.extend(module_files(&input[end + 1..body_end], &nested, &nested));
            i = body_end + 1;
        }
    }
    (output, test_files, children)
}

fn source_files(directory: &Path, output: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            source_files(&path, output);
        } else if path.extension().is_some_and(|e| e == "rs") {
            output.push(path);
        }
    }
}

fn scan(root: &Path, allowlist: &[&str]) -> Result<(), String> {
    let normalized_root = normalized_module_path(root.to_path_buf());
    let root = normalized_root.as_path();
    let mut fixed: BTreeSet<String> = ["windows.rs", "macos.rs", "tenant_agent.rs"]
        .map(String::from)
        .into_iter()
        .collect();
    let mut macos = Vec::new();
    source_files(&root.join("macos"), &mut macos);
    fixed.extend(macos.iter().map(|path| {
        path.strip_prefix(root)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }));
    let supplied: BTreeSet<String> = allowlist.iter().map(|s| (*s).to_owned()).collect();
    if supplied != fixed || supplied.len() != allowlist.len() {
        return Err("caller-path allowlist differs from the fixed file set".into());
    }
    let mut files = Vec::new();
    source_files(root, &mut files);
    let mut excluded = BTreeSet::new();
    let mut modules = BTreeMap::new();
    let mut production_modules = BTreeMap::new();
    let mut sources = Vec::new();
    for path in files {
        let (tokens, tests, children) =
            production_tokens(&fs::read_to_string(&path).unwrap(), &path);
        excluded.extend(tests);
        let relative = path.strip_prefix(root).unwrap();
        if relative == Path::new("tests.rs") || relative.starts_with("tests") {
            excluded.insert(path.clone());
        }
        modules.insert(path.clone(), children);
        production_modules.insert(
            path.clone(),
            module_files(&tokens, &module_directory(&path), path.parent().unwrap()),
        );
        sources.push((path, tokens));
    }
    // A module reached only through a test module is also test-only, even
    // when its own declaration has no cfg(test) attribute.
    let mut pending: Vec<_> = excluded.iter().cloned().collect();
    while let Some(path) = pending.pop() {
        if let Some(children) = modules.get(&path) {
            for child in children {
                if excluded.insert(child.clone()) {
                    pending.push(child.clone());
                }
            }
        }
    }
    // A shared source reached by production as well as by tests must still be
    // scanned; a test-module reference must not hide a production consumer.
    let mut production = BTreeSet::new();
    let mut pending: Vec<_> = sources
        .iter()
        .filter(|(path, _)| !excluded.contains(path))
        .map(|(path, _)| path.clone())
        .collect();
    while let Some(path) = pending.pop() {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        if relative == Path::new("tests.rs") || relative.starts_with("tests") {
            continue;
        }
        if production.insert(path.clone()) {
            if let Some(children) = production_modules.get(&path) {
                pending.extend(children.iter().cloned());
            }
        }
    }
    excluded.retain(|path| !production.contains(path));
    let mut hits = Vec::new();
    for (path, tokens) in sources {
        let relative = path.strip_prefix(root).unwrap().to_str().unwrap();
        if relative == "caller_path.rs"
            || relative == "tests.rs"
            || relative.starts_with("tests/")
            || excluded.contains(&path)
            || supplied.contains(relative)
        {
            continue;
        }
        for (index, pair) in tokens.windows(2).enumerate() {
            // Job::key() takes no arguments. CacheDisks::key(workspace) and
            // credential-key helpers are different APIs, not job-record reads.
            if pair[0].text == "."
                && matches!(
                    pair[1].text.as_str(),
                    "workspace_key" | "repository_root" | "cwd" | "canonical_path" | "return_paths"
                )
                || (pair[0].text == "."
                    && pair[1].text == "key"
                    && tokens.get(index + 2).is_some_and(|t| t.text == "(")
                    && tokens.get(index + 3).is_some_and(|t| t.text == ")"))
            {
                hits.push(format!("{relative}:{} .{}", pair[1].line, pair[1].text));
            }
        }
        for (index, token) in tokens.iter().enumerate() {
            if token.text != "catalog"
                || tokens.get(index + 1).is_none_or(|t| t.text != ".")
                || tokens.get(index + 2).is_none_or(|t| {
                    !matches!(
                        t.text.as_str(),
                        "workspaces" | "repositories" | "legacy_checkouts" | "sibling_commits"
                    )
                })
            {
                continue;
            }
            let tail = &tokens[index + 3..];
            let method = tail.first().is_some_and(|t| t.text == ".")
                && tail.get(1).is_some_and(|t| {
                    matches!(
                        t.text.as_str(),
                        "keys" | "iter" | "iter_mut" | "into_iter" | "into_keys"
                    )
                });
            let direct_loop = tail.first().is_some_and(|t| t.text == "{")
                && tokens[index.saturating_sub(8)..index]
                    .iter()
                    .any(|t| t.text == "in");
            if method || direct_loop {
                hits.push(format!("{relative}:{} catalog key read", token.line));
            }
        }
    }
    if hits.is_empty() {
        Ok(())
    } else {
        Err(hits.join("\n"))
    }
}

fn check_test_module_graph() {
    let scratch = cortexkit_test_support::ScratchDir::new("motor-path-backstop-modules");
    for file in ALLOWLIST {
        let path = scratch.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }
    fs::create_dir_all(scratch.join("support/nested")).unwrap();
    fs::create_dir_all(scratch.join("inline")).unwrap();
    let root = "#[cfg(test)] #[path = \"support/test_root.rs\"] pub(crate) mod tests;\n#[cfg(test)] mod inline { #[path = \"child.rs\"] pub mod child; }\nfn production() { cache.key(workspace); }\n";
    fs::write(scratch.join("lib.rs"), root).unwrap();
    fs::write(
        scratch.join("support/test_root.rs"),
        "#[path = \"nested/child.rs\"] pub mod child; #[path = \"nested/shared.rs\"] mod shared;",
    )
    .unwrap();
    fs::write(
        scratch.join("support/nested/child.rs"),
        "fn test() { request.cwd; }",
    )
    .unwrap();
    fs::write(
        scratch.join("support/nested/shared.rs"),
        "fn test() { request.repository_root; }",
    )
    .unwrap();
    fs::write(
        scratch.join("inline/child.rs"),
        "fn test() { sibling.canonical_path; }",
    )
    .unwrap();
    scan(&scratch, ALLOWLIST).unwrap();
    fs::write(
        scratch.join("lib.rs"),
        format!("{root}#[path = \"support/nested/shared.rs\"] mod production_shared;\n"),
    )
    .unwrap();
    let error = scan(&scratch, ALLOWLIST).unwrap_err();
    assert!(
        error.contains("support/nested/shared.rs") && error.contains(".repository_root"),
        "{error}"
    );
}

#[test]
fn production_path_reads_use_the_typed_boundary() {
    scan(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        ALLOWLIST,
    )
    .unwrap();
}

#[test]
fn backstop_rejects_planted_reads_and_allowlist_drift_but_accepts_test_reads() {
    check_test_module_graph();
    let scratch = cortexkit_test_support::ScratchDir::new("motor-path-backstop");
    for file in ALLOWLIST {
        let path = scratch.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }
    fs::create_dir_all(scratch.join("tests")).unwrap();
    fs::write(scratch.join("tests.rs"), "fn test() { r.cwd; }").unwrap();
    fs::write(scratch.join("tests/fixture.rs"), "fn test() { r.cwd; }").unwrap();
    fs::write(
        scratch.join("production.rs"),
        "#[cfg(test)] mod tests { fn test() { r.cwd; } }\nfn real() { let _ = r.command; }",
    )
    .unwrap();
    scan(&scratch, ALLOWLIST).unwrap();
    for field in [
        "workspace_key",
        "repository_root",
        "cwd",
        "canonical_path",
        "return_paths",
    ] {
        fs::write(
            scratch.join("production.rs"),
            format!("fn real() {{ r /* gap */ . {field}; }}"),
        )
        .unwrap();
        let error = scan(&scratch, ALLOWLIST).unwrap_err();
        assert!(error.contains(&format!(".{field}")), "{error}");
    }
    for source in [
        "fn real() { job.key(); }",
        "fn real() { inner.catalog.workspaces.keys(); }",
        "fn real() { catalog.repositories.iter(); }",
        "fn real() { for (key, record) in &catalog.legacy_checkouts {} }",
        "#[cfg(all(any(test, unix)))] mod production { fn real() { r.cwd; } }",
    ] {
        fs::write(scratch.join("production.rs"), source).unwrap();
        assert!(scan(&scratch, ALLOWLIST).is_err(), "{source}");
    }
    fs::write(scratch.join("production.rs"), "").unwrap();
    // Terminal evidence once had its own exception for a fill's key; it now
    // reads the probed path through caller_path like every other consumer.
    for source in [
        "fn execution_evidence() { job.key(); }",
        "fn execution_evidence() { request.cwd; }",
    ] {
        fs::write(scratch.join("terminal_evidence.rs"), source).unwrap();
        assert!(scan(&scratch, ALLOWLIST).is_err(), "{source}");
    }
    fs::remove_file(scratch.join("terminal_evidence.rs")).unwrap();
    assert!(scan(&scratch, &ALLOWLIST[1..])
        .unwrap_err()
        .contains("fixed file set"));
    let mut added = ALLOWLIST.to_vec();
    added.push("production.rs");
    assert!(scan(&scratch, &added)
        .unwrap_err()
        .contains("fixed file set"));
}
