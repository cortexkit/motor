use super::{fs::Dir, Control, Trees};
use crate::storage::invalid;
use motor_protocol::GitDependency;
use std::{
    collections::BTreeMap,
    io,
    path::{Component, Path, PathBuf},
};

fn https_git(source: &str) -> bool {
    url::Url::parse(source).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
    })
}
fn local(source: &str, base: &Path, roots: &[PathBuf]) -> io::Result<PathBuf> {
    if Path::new(source).is_absolute() || source.contains(['\\', '\0']) {
        return Err(invalid(format!(
            "absolute or invalid local dependency: {source}"
        )));
    }
    let mut resolved = base.to_path_buf();
    for c in Path::new(source).components() {
        match c {
            Component::Normal(n) => resolved.push(n),
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            _ => return Err(invalid(format!("invalid local dependency: {source}"))),
        }
    }
    if !roots.iter().any(|root| resolved.starts_with(root)) {
        return Err(invalid(format!("out-of-tree local dependency: {source}")));
    }
    Dir::open(&resolved, false).map_err(|e| invalid(format!("local dependency {source}: {e}")))?;
    Ok(resolved)
}
fn cargo(value: &toml::Value, base: &Path, roots: &[PathBuf]) -> io::Result<()> {
    if let Some(workspace) = value
        .get("package")
        .and_then(|package| package.get("workspace"))
        .and_then(toml::Value::as_str)
    {
        local(workspace, base, roots)?;
    }
    cargo_values(value, base, roots)
}
fn cargo_values(value: &toml::Value, base: &Path, roots: &[PathBuf]) -> io::Result<()> {
    if let Some(table) = value.as_table() {
        if let Some(workspace) = table.get("workspace").and_then(toml::Value::as_table) {
            for key in ["members", "default-members", "exclude"] {
                if let Some(paths) = workspace.get(key).and_then(toml::Value::as_array) {
                    for path in paths {
                        workspace_path(
                            path.as_str()
                                .ok_or_else(|| invalid("invalid cargo workspace member"))?,
                            base,
                            roots,
                        )?;
                    }
                }
            }
        }
        if let Some(path) = table.get("path").and_then(toml::Value::as_str) {
            // Target source files are allowed as well as dependency directories.
            let path_to_check = if path.ends_with(".rs") {
                Path::new(path)
                    .parent()
                    .unwrap_or(Path::new("."))
                    .to_str()
                    .unwrap_or("")
            } else {
                path
            };
            local(path_to_check, base, roots)?;
            if Path::new(path).is_absolute() {
                return Err(invalid(format!("absolute local dependency: {path}")));
            }
        }
        if let Some(source) = table.get("git").and_then(toml::Value::as_str) {
            if !https_git(source) {
                return Err(invalid(format!("unsupported dependency source: {source}")));
            }
        }
        if let Some(source) = table.get("registry").and_then(toml::Value::as_str) {
            if source != "crates-io" {
                return Err(invalid(format!("unsupported registry: {source}")));
            }
        }
        for child in table.values() {
            cargo_values(child, base, roots)?;
        }
    } else if let Some(array) = value.as_array() {
        for child in array {
            cargo_values(child, base, roots)?;
        }
    }
    Ok(())
}
fn lock(text: &str) -> io::Result<()> {
    let value: toml::Value = toml::from_str(text).map_err(invalid)?;
    for p in value
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(source) = p.get("source").and_then(toml::Value::as_str) {
            if matches!(
                source,
                "registry+https://github.com/rust-lang/crates.io-index"
                    | "sparse+https://index.crates.io/"
            ) {
                continue;
            }
            if source.strip_prefix("git+").is_some_and(https_git) {
                continue;
            }
            return Err(invalid(format!(
                "unsupported locked dependency source: {source}"
            )));
        }
    }
    Ok(())
}
fn npm(source: &str, base: &Path, roots: &[PathBuf]) -> io::Result<()> {
    if let Some(relative) = source
        .strip_prefix("file:")
        .or_else(|| source.strip_prefix("link:"))
    {
        local(relative, base, roots)
            .map_err(|e| invalid(format!("package source {source}: {e}")))?;
        return Ok(());
    }
    if let Some(relative) = source.strip_prefix("workspace:") {
        if matches!(relative, "*" | "^" | "~")
            || relative
                .bytes()
                .all(|c| c.is_ascii_digit() || b".^~*>=<| -".contains(&c))
        {
            return Ok(());
        }
        local(relative, base, roots)?;
        return Ok(());
    }
    // An npm alias (`npm:<name>@<range>`) also parses as a URL with an `npm`
    // scheme, so it is checked as a registry range below, not as a URL.
    if let Some(url) = (!source.starts_with("npm:"))
        .then(|| url::Url::parse(source).ok())
        .flatten()
    {
        if url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && (matches!(
                url.host_str(),
                Some("registry.npmjs.org" | "registry.yarnpkg.com" | "codeload.github.com")
            ) || (url.host_str() == Some("github.com") && url.path().contains("/archive/")))
        {
            return Ok(());
        }
        return Err(invalid(format!("unsupported package dependency: {source}")));
    }
    let range = source.strip_prefix("npm:").unwrap_or(source);
    if !range.is_empty()
        && range
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"@/._-*^~><=| +".contains(&c))
        && (!range.contains('/') || source.starts_with("npm:"))
        && !range.starts_with('.')
    {
        return Ok(());
    }
    Err(invalid(format!("unsupported package dependency: {source}")))
}
fn package(value: &serde_json::Value, base: &Path, roots: &[PathBuf]) -> io::Result<()> {
    if let Some(workspaces) = value.get("workspaces") {
        let entries = workspaces
            .as_array()
            .or_else(|| {
                workspaces
                    .get("packages")
                    .and_then(serde_json::Value::as_array)
            })
            .ok_or_else(|| invalid("invalid package workspaces"))?;
        for entry in entries {
            workspace_path(
                entry
                    .as_str()
                    .ok_or_else(|| invalid("invalid package workspace member"))?,
                base,
                roots,
            )?;
        }
    }
    for key in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
        "overrides",
        "resolutions",
    ] {
        if let Some(deps) = value.get(key).and_then(serde_json::Value::as_object) {
            for (name, version) in deps {
                npm_values(version, base, roots)
                    .map_err(|e| invalid(format!("package {name}: {e}")))?;
            }
        }
    }
    Ok(())
}
fn workspace_path(source: &str, base: &Path, roots: &[PathBuf]) -> io::Result<()> {
    let pattern = source.strip_prefix('!').unwrap_or(source);
    if let Some(index) = pattern.find(['*', '?', '[', '{']) {
        let (prefix, suffix) = pattern.split_at(index);
        if suffix.split('/').any(|p| p == "..") {
            return Err(invalid(format!("out-of-tree workspace pattern: {source}")));
        }
        let parent =
            prefix.rsplit_once('/').map_or(
                ".",
                |(parent, _)| if parent.is_empty() { "/" } else { parent },
            );
        local(parent, base, roots).map_err(|e| invalid(format!("workspace {source}: {e}")))?;
    } else {
        local(pattern, base, roots).map_err(|e| invalid(format!("workspace {source}: {e}")))?;
    }
    Ok(())
}
fn npm_values(value: &serde_json::Value, base: &Path, roots: &[PathBuf]) -> io::Result<()> {
    match value {
        serde_json::Value::String(s) => npm(s, base, roots),
        serde_json::Value::Object(o) => {
            for v in o.values() {
                npm_values(v, base, roots)?;
            }
            Ok(())
        }
        _ => Err(invalid("invalid dependency specification")),
    }
}
// Bun's text lock is JSON with comments and trailing commas, not arbitrary JS.
fn bun_json(text: &str) -> io::Result<serde_json::Value> {
    let mut bytes = text.as_bytes().to_vec();
    let mut i = 0;
    let mut quoted = false;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            quoted = !quoted;
            i += 1;
            continue;
        }
        if quoted && bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if !quoted && bytes.get(i..i + 2) == Some(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                bytes[i] = b' ';
                i += 1;
            }
            continue;
        }
        if !quoted && bytes.get(i..i + 2) == Some(b"/*") {
            bytes[i] = b' ';
            bytes[i + 1] = b' ';
            i += 2;
            while i + 1 < bytes.len() && &bytes[i..i + 2] != b"*/" {
                bytes[i] = b' ';
                i += 1;
            }
            if i + 1 >= bytes.len() {
                return Err(invalid("unterminated bun lock comment"));
            }
            bytes[i] = b' ';
            bytes[i + 1] = b' ';
            i += 2;
            continue;
        }
        i += 1;
    }
    quoted = false;
    i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            quoted = !quoted;
        } else if quoted && bytes[i] == b'\\' {
            i += 2;
            continue;
        } else if !quoted && bytes[i] == b',' {
            let next = bytes[i + 1..].iter().find(|c| !c.is_ascii_whitespace());
            if matches!(next, Some(b']' | b'}')) {
                bytes[i] = b' ';
            }
        }
        i += 1;
    }
    serde_json::from_slice(&bytes).map_err(invalid)
}
fn bun(value: &serde_json::Value, base: &Path, roots: &[PathBuf]) -> io::Result<()> {
    if let Some(workspaces) = value
        .get("workspaces")
        .and_then(serde_json::Value::as_object)
    {
        for (path, workspace) in workspaces {
            let base = if path.is_empty() {
                base.to_path_buf()
            } else {
                local(path, base, roots)?
            };
            package(workspace, &base, roots)?;
        }
    }
    if let Some(packages) = value.get("packages").and_then(serde_json::Value::as_object) {
        for (name, tuple) in packages {
            let tuple = tuple
                .as_array()
                .ok_or_else(|| invalid(format!("invalid bun lock package: {name}")))?;
            let spec = tuple
                .first()
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| invalid(format!("invalid bun lock package: {name}")))?;
            let source = spec
                .strip_prefix('@')
                .unwrap_or(spec)
                .split_once('@')
                .map_or(spec, |(_, version)| version);
            npm(source, base, roots).map_err(|e| invalid(format!("locked package {name}: {e}")))?;
            for metadata in tuple.iter().filter(|v| v.is_object()) {
                package(metadata, base, roots)?;
            }
            if let Some(resolved) = tuple
                .get(1)
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
            {
                npm(resolved, base, roots)?;
            }
        }
    }
    Ok(())
}
pub(super) fn validate(
    roots: &[PathBuf],
    deps: &[GitDependency],
    trees: &Trees,
    control: &Control,
) -> io::Result<BTreeMap<String, PathBuf>> {
    fn walk(
        dir: &Dir,
        base: &Path,
        roots: &[PathBuf],
        deps: &[GitDependency],
        trees: &Trees,
        routes: &mut BTreeMap<String, PathBuf>,
        control: &Control,
    ) -> io::Result<()> {
        for name in dir.names()? {
            control.check()?;
            let path = base.join(&name);
            if dir.kind(&name)? == libc::S_IFDIR {
                walk(
                    &dir.child(&name, false)?,
                    &path,
                    roots,
                    deps,
                    trees,
                    routes,
                    control,
                )?;
                continue;
            }
            match name.to_str() {
                Some("Cargo.toml") => cargo(
                    &toml::from_str::<toml::Value>(&dir.text(&name)?).map_err(invalid)?,
                    base,
                    roots,
                )?,
                Some("Cargo.lock") => {
                    let text = dir.text(&name)?;
                    lock(&text)?;
                    let (found, missing) =
                        trees.dependency_fetch_routes_checked(deps, &text, Some(&|| control.check()))?;
                    if !missing.is_empty() {
                        return Err(invalid(missing.join("; ")));
                    }
                    routes.extend(found);
                }
                Some("package.json") => package(
                    &serde_json::from_str::<serde_json::Value>(&dir.text(&name)?)
                        .map_err(invalid)?,
                    base,
                    roots,
                )?,
                Some("bun.lock") => bun(&bun_json(&dir.text(&name)?)?, base, roots)?,
                Some("bun.lockb") => {
                    return Err(invalid(format!(
                        "unsupported binary Bun lockfile: {}; convert it with `bun install --save-text-lockfile` and commit bun.lock",
                        path.display()
                    )))
                }
                _ => {}
            }
        }
        Ok(())
    }
    let mut routes = BTreeMap::new();
    for root in roots {
        walk(
            &Dir::open(root, false)?,
            root,
            roots,
            deps,
            trees,
            &mut routes,
            control,
        )?;
    }
    Ok(routes)
}

pub(super) fn validate_links(root: &Path, roots: &[PathBuf], control: &Control) -> io::Result<()> {
    fn walk(
        dir: &Dir,
        base: &Path,
        roots: &[PathBuf],
        control: &Control,
        depth: usize,
    ) -> io::Result<()> {
        if depth >= super::fs::MAX_TREE_DEPTH {
            return Err(invalid("node_modules tree depth limit exceeded"));
        }
        for name in dir.names()? {
            control.check()?;
            let path = base.join(&name);
            match dir.kind(&name)? {
                libc::S_IFDIR => walk(&dir.child(&name, false)?, &path, roots, control, depth + 1)?,
                libc::S_IFLNK => {
                    let target = dir.link(&name)?;
                    let text = target.to_str().ok_or_else(|| {
                        invalid(format!("non-UTF8 node_modules link: {}", path.display()))
                    })?;
                    resolve_link(lexical_link(text, base, roots, &path)?, roots, &path)?;
                }
                libc::S_IFREG => {}
                _ => {
                    return Err(invalid(format!(
                        "special node_modules file: {}",
                        path.display()
                    )))
                }
            }
        }
        Ok(())
    }
    for installation in installations(root, control)? {
        walk(
            &Dir::open(&installation, false)?,
            &installation,
            roots,
            control,
            0,
        )?;
    }
    Ok(())
}
pub(super) fn installations(root: &Path, control: &Control) -> io::Result<Vec<PathBuf>> {
    fn walk(
        dir: &Dir,
        base: &Path,
        found: &mut Vec<PathBuf>,
        control: &Control,
        depth: usize,
    ) -> io::Result<()> {
        if depth >= super::fs::MAX_TREE_DEPTH {
            return Err(invalid("installation tree depth limit exceeded"));
        }
        for name in dir.names()? {
            control.check()?;
            let path = base.join(&name);
            if name == "node_modules" {
                dir.child(&name, false).map_err(|e| {
                    invalid(format!("node_modules directory {}: {e}", path.display()))
                })?;
                found.push(path);
            } else if dir.kind(&name)? == libc::S_IFDIR {
                walk(&dir.child(&name, false)?, &path, found, control, depth + 1)?;
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    walk(&Dir::open(root, false)?, root, &mut found, control, 0)?;
    Ok(found)
}
fn lexical_link(text: &str, base: &Path, roots: &[PathBuf], link: &Path) -> io::Result<PathBuf> {
    if Path::new(text).is_absolute() {
        return Err(invalid(format!(
            "absolute node_modules link: {} -> {text}",
            link.display()
        )));
    }
    let mut target = base.to_path_buf();
    for part in Path::new(text).components() {
        match part {
            Component::Normal(n) => target.push(n),
            Component::ParentDir => {
                target.pop();
            }
            Component::CurDir => {}
            _ => return Err(invalid("invalid node_modules link")),
        }
    }
    if !roots.iter().any(|root| target.starts_with(root)) {
        return Err(invalid(format!(
            "escaping node_modules link: {} -> {text}",
            link.display()
        )));
    }
    Ok(target)
}

fn resolve_link(mut target: PathBuf, roots: &[PathBuf], link: &Path) -> io::Result<()> {
    // Inspect every hop, including symlinks in ancestors, without asking the kernel to follow one.
    'chain: for _ in 0..40 {
        let components: Vec<_> = target
            .components()
            .filter_map(|c| {
                if let Component::Normal(n) = c {
                    Some(n.to_os_string())
                } else {
                    None
                }
            })
            .collect();
        let mut dir = Dir::open(Path::new("/"), false)?;
        let mut prefix = PathBuf::from("/");
        for (index, name) in components.iter().enumerate() {
            let kind = dir
                .kind(name)
                .map_err(|e| invalid(format!("node_modules link {}: {e}", link.display())))?;
            if kind == libc::S_IFLNK {
                let text = dir.link(name)?;
                target = lexical_link(
                    text.to_str()
                        .ok_or_else(|| invalid("non-UTF8 node_modules link"))?,
                    &prefix,
                    roots,
                    link,
                )?;
                for remaining in &components[index + 1..] {
                    target.push(remaining);
                }
                continue 'chain;
            }
            prefix.push(name);
            if index + 1 < components.len() {
                dir = dir
                    .child(name, false)
                    .map_err(|e| invalid(format!("node_modules link {}: {e}", link.display())))?;
            }
        }
        return Ok(());
    }
    Err(invalid(format!(
        "cyclic node_modules link: {}",
        link.display()
    )))
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    #[test]
    fn deep_node_modules_are_refused_before_unbounded_walk() {
        let scratch = cortexkit_test_support::ScratchDir::new("deep-module-validation");
        let root = scratch.join("tree");
        let modules = root.join("node_modules");
        let mut dir = Dir::open(&modules, true).unwrap();
        for _ in 0..(super::super::fs::MAX_TREE_DEPTH + 16) {
            dir = dir.child("d".as_ref(), true).unwrap();
        }
        drop(dir);
        let control = super::super::Control {
            cancel: super::super::Cancellation::default(),
            deadline: std::time::Instant::now() + super::super::PREP_TIMEOUT,
        };
        assert!(validate_links(&root, std::slice::from_ref(&root), &control)
            .unwrap_err()
            .to_string()
            .contains("depth limit"));
    }
    #[test]
    fn unterminated_bun_block_comments_are_refusals_not_panics() {
        for text in ["/*", "{} /*", "/*x", "{\"packages\":{}} /*"] {
            let result = std::panic::catch_unwind(|| bun_json(text));
            let error = result
                .expect("untrusted lockfile must not panic")
                .unwrap_err();
            assert!(error.to_string().contains("unterminated"), "{error}");
        }
        assert!(bun_json("{} /* closed */").is_ok());
    }
    #[test]
    fn package_workspace_string_cannot_leave_validated_prep_roots() {
        let scratch = cortexkit_test_support::ScratchDir::new("package-workspace-preflight");
        let root = scratch.join("tree");
        std::fs::create_dir(&root).unwrap();
        for workspace in ["/outside", "../../outside", "../outside"] {
            let value: toml::Value = toml::from_str(&format!(
                "[package]\nname='test'\nworkspace={workspace:?}\n"
            ))
            .unwrap();
            let error = cargo(&value, &root, std::slice::from_ref(&root))
                .unwrap_err()
                .to_string();
            assert!(error.contains(workspace), "{error}");
        }
        let value: toml::Value = toml::from_str("[package]\nname='test'\nworkspace='.'\n").unwrap();
        cargo(&value, &root, std::slice::from_ref(&root)).unwrap();
        let metadata:toml::Value=toml::from_str("[package]\nname='test'\n[package.metadata]\nworkspace='/not-a-cargo-workspace-pointer'\n").unwrap();
        cargo(&metadata, &root, std::slice::from_ref(&root)).unwrap();
    }
}

#[cfg(test)]
mod npm_source_tests {
    use super::npm;
    use std::path::Path;

    #[test]
    fn npm_aliases_are_registry_ranges_and_other_schemes_are_refused() {
        let base = Path::new("/Users/motor/main");
        for accepted in [
            "npm:string-width@^4.2.0",
            "npm:@scope/name@1.2.3",
            "^4.2.0",
            "https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz",
        ] {
            npm(accepted, base, &[]).unwrap_or_else(|e| panic!("{accepted}: {e}"));
        }
        for refused in [
            "npm:../outside",
            "ssh://host/repo.git",
            "http://registry.npmjs.org/x.tgz",
        ] {
            assert!(npm(refused, base, &[]).is_err(), "{refused} accepted");
        }
    }
}
