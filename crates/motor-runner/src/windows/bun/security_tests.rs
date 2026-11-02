use super::*;
use cortexkit_test_support::ScratchDir;
use motor_winagent::bun::{expand_tarball_with_budget, prepare_cache_with_budget, ExpansionBudget};
use std::{
    io::Write,
    time::{Duration, Instant},
};

fn package_tar(name: &str) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let json = format!(r#"{{"name":"{name}","version":"1.0.0"}}"#);
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(json.len() as u64);
    header.set_cksum();
    tar.append_data(&mut header, "package/package.json", json.as_bytes())
        .unwrap();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&tar.into_inner().unwrap()).unwrap();
    gzip.finish().unwrap()
}

#[test]
fn graph_rejects_workspace_alias_multiplication_and_excess_workspaces() {
    let mut aliases = serde_json::Map::new();
    for i in 0..1000 {
        aliases.insert(
            format!("alias{i}"),
            serde_json::json!(["workspace@workspace:packages/w"]),
        );
    }
    let dependencies: serde_json::Map<String, serde_json::Value> = (0..100)
        .map(|i| (format!("dep{i}"), serde_json::json!("1.0.0")))
        .collect();
    let lock = serde_json::json!({"lockfileVersion":1,"workspaces":{"":{},"packages/w":{"name":"workspace","dependencies":dependencies}},"packages":aliases});
    assert_eq!(
        bun::read_lock("m/project", &lock.to_string())
            .unwrap_err()
            .kind,
        RefusalKind::BunLockInvalid
    );
    let mut workspaces = serde_json::Map::new();
    workspaces.insert("".into(), serde_json::json!({}));
    for i in 0..128 {
        workspaces.insert(format!("p{i}"), serde_json::json!({"name":format!("p{i}")}));
    }
    let lock = serde_json::json!({"lockfileVersion":1,"workspaces":workspaces,"packages":{}});
    assert_eq!(
        bun::read_lock("m/project", &lock.to_string())
            .unwrap_err()
            .kind,
        RefusalKind::BunLockInvalid
    );
}

/// A pax record as `<len> <key>=<value>\n`, where `len` counts the whole record.
fn pax_record(key: &str, value: &str) -> String {
    let body = format!(" {key}={value}\n");
    let mut length = body.len() + 1;
    while length.to_string().len() + body.len() != length {
        length += 1;
    }
    format!("{length}{body}")
}

/// npm's tar shape for a long file name: a pax `x` header carrying `path`
/// (and, from npm, `size`) before a ustar member with a truncated name.
fn pax_tar(long_name: &str, contents: &[u8], extra: &[(&str, String)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let json = br#"{"name":"pax","version":"1.0.0"}"#;
    let mut header = tar::Header::new_ustar();
    header.set_mode(0o644);
    header.set_size(json.len() as u64);
    header.set_path("package/package.json").unwrap();
    header.set_cksum();
    tar.append(&header, &json[..]).unwrap();
    let mut pax = pax_record("path", &format!("package/{long_name}"));
    for (key, value) in extra {
        pax.push_str(&pax_record(key, value));
    }
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::XHeader);
    header.set_size(pax.len() as u64);
    header.set_path("PaxHeader/long").unwrap();
    header.set_cksum();
    tar.append(&header, pax.as_bytes()).unwrap();
    let mut header = tar::Header::new_ustar();
    header.set_mode(0o644);
    header.set_size(contents.len() as u64);
    header.set_path("package/truncated").unwrap();
    header.set_cksum();
    tar.append(&header, contents).unwrap();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&tar.into_inner().unwrap()).unwrap();
    gzip.finish().unwrap()
}

/// npm writes a pax `size` beside every long `path` (seen on
/// @mistralai/mistralai 2.2.6). An equal size is the header's own and must
/// expand; a different size, or sparse-file records, stay refused.
#[test]
fn pax_size_equal_to_the_header_expands_and_others_are_refused() {
    let scratch = ScratchDir::new("bun-pax-size");
    let long = "getchatcompletionfieldoptionscountsv1observabilitychatcompletionfieldsfieldnameoptionscountsrequest.d.ts";
    let budget = || ExpansionBudget::new(1 << 30, 1000, Instant::now() + Duration::from_secs(30));
    let equal = pax_tar(
        long,
        b"export {};\n",
        &[("size", "11".into()), ("mtime", "1.5".into())],
    );
    let destination = scratch.join("equal");
    expand_tarball_with_budget(&equal, &destination, &budget()).unwrap();
    assert_eq!(
        std::fs::read(destination.join(long)).unwrap(),
        b"export {};\n"
    );
    for (case, extra) in [
        ("differs", vec![("size", "12".to_string())]),
        ("sparse", vec![("GNU.sparse.size", "11".to_string())]),
        ("realsize", vec![("SCHILY.realsize", "11".to_string())]),
    ] {
        let destination = scratch.join(case);
        assert!(
            expand_tarball_with_budget(
                &pax_tar(long, b"export {};\n", &extra),
                &destination,
                &budget()
            )
            .is_err(),
            "{case} must be refused"
        );
        assert!(!destination.exists(), "{case} wrote files");
    }
}

fn tar_of(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, contents) in members {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o644);
        header.set_size(contents.len() as u64);
        // set_path refuses `.` components, which real npm tarballs contain.
        header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        tar.append(&header, *contents).unwrap();
    }
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&tar.into_inner().unwrap()).unwrap();
    gzip.finish().unwrap()
}

/// agent-base 7.1.4 packs `package/./dist/index.js` and an identical
/// `package/dist/index.js`. `.` is the same folder and an identical repeat is
/// harmless; a repeat with other bytes, or `..`, stays refused.
#[test]
fn dot_components_and_identical_repeats_expand_but_divergent_ones_do_not() {
    let scratch = ScratchDir::new("bun-dot-members");
    let budget = || ExpansionBudget::new(1 << 30, 1000, Instant::now() + Duration::from_secs(30));
    let json: &[u8] = br#"{"name":"dot","version":"1.0.0"}"#;
    let same = tar_of(&[
        ("package/package.json", json),
        ("package/./dist/index.js", b"one"),
        ("package/dist/index.js", b"one"),
    ]);
    let destination = scratch.join("same");
    expand_tarball_with_budget(&same, &destination, &budget()).unwrap();
    assert_eq!(
        std::fs::read(destination.join("dist/index.js")).unwrap(),
        b"one"
    );
    for (case, members) in [
        (
            "differs",
            vec![
                ("package/package.json", json),
                ("package/./dist/index.js", &b"one"[..]),
                ("package/dist/index.js", &b"two"[..]),
            ],
        ),
        (
            "parent",
            vec![
                ("package/package.json", json),
                ("package/../escape.js", &b"x"[..]),
            ],
        ),
    ] {
        let destination = scratch.join(case);
        assert!(
            expand_tarball_with_budget(&tar_of(&members), &destination, &budget()).is_err(),
            "{case} must be refused"
        );
    }
    assert!(!scratch.join("escape.js").exists());
}

/// Operator check against real registry tarballs: expands every `*.tgz` in
/// `MOTOR_BUN_TARBALL_DIR` under one job budget and lists the ones refused.
#[test]
#[ignore = "needs MOTOR_BUN_TARBALL_DIR with real npm tarballs"]
fn real_npm_tarballs_expand_under_one_job_budget() {
    let directory = std::env::var("MOTOR_BUN_TARBALL_DIR").unwrap();
    let scratch = ScratchDir::new("bun-real-tarballs");
    let budget = ExpansionBudget::default();
    let mut refused = Vec::new();
    let mut count = 0;
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "tgz") {
            continue;
        }
        count += 1;
        let bytes = std::fs::read(&path).unwrap();
        let destination = scratch.join(format!("p{count}"));
        if let Err(error) = expand_tarball_with_budget(&bytes, &destination, &budget) {
            refused.push(format!("{}: {error}", path.display()));
        }
    }
    assert!(count > 0, "no tarballs found");
    assert!(
        refused.is_empty(),
        "{} of {count} refused:\n{}",
        refused.len(),
        refused.join("\n")
    );
}

#[test]
fn expansion_budget_is_cumulative_across_tarballs() {
    let scratch = ScratchDir::new("bun-stream-budget");
    let bytes = package_tar("first");
    for (kind, budget) in [
        (
            "bytes",
            ExpansionBudget::new(3000, 100, Instant::now() + Duration::from_secs(30)),
        ),
        (
            "entries",
            ExpansionBudget::new(1_000_000, 1, Instant::now() + Duration::from_secs(30)),
        ),
    ] {
        expand_tarball_with_budget(&bytes, &scratch.join(format!("{kind}-first")), &budget)
            .unwrap();
        let second = scratch.join(format!("{kind}-second"));
        assert!(expand_tarball_with_budget(&bytes, &second, &budget).is_err());
        assert!(!second.exists());
    }
}

#[test]
fn cache_refusal_cleans_prior_packages_and_honors_early_deadline() {
    use base64::Engine;
    let scratch = ScratchDir::new("bun-cache-cleanup");
    let source = scratch.join("source/project");
    let seed = scratch.join("seed");
    fs::create_dir_all(&source).unwrap();
    let mut rows = serde_json::Map::new();
    for name in ["first", "second"] {
        let bytes = package_tar(name);
        let hex = hex_sha256(&bytes);
        let path = seed.join("blobs/sha256").join(&hex);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &bytes).unwrap();
        rows.insert(
            name.into(),
            serde_json::json!([
                format!("{name}@1.0.0"),
                "",
                {},
                format!(
                    "sha256-{}",
                    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes))
                )
            ]),
        );
    }
    let lock = serde_json::json!({"lockfileVersion":1,"workspaces":{"":{"dependencies":{"first":"1.0.0","second":"1.0.0"}}},"packages":rows}).to_string();
    fs::write(source.join("bun.lock"), &lock).unwrap();
    fs::write(source.join("package.json"), "{}").unwrap();
    let manifest = Manifest {
        format: 1,
        bun_version: bun::BUN_VERSION.into(),
        registries: BTreeMap::new(),
        members: Vec::new(),
        projects: vec![Project {
            skipped: None,
            path: "m/project".into(),
            lock_sha256: hex_sha256(lock.as_bytes()),
            manifest_sha256: hex_sha256(b"{}"),
            name: String::new(),
            version: String::new(),
            packages: bun::read_lock("m/project", &lock)
                .unwrap()
                .into_iter()
                .map(|p| p.package)
                .collect(),
        }],
    };
    fs::write(
        seed.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    for (name, budget) in [
        (
            "bytes",
            ExpansionBudget::new(3000, 100, Instant::now() + Duration::from_secs(30)),
        ),
        (
            "entries",
            ExpansionBudget::new(1_000_000, 1, Instant::now() + Duration::from_secs(30)),
        ),
        (
            "expired",
            ExpansionBudget::new(1_000_000, 100, Instant::now() - Duration::from_secs(1)),
        ),
    ] {
        let cache = scratch.join(name);
        assert_eq!(
            prepare_cache_with_budget(&seed, &scratch.join("source"), &cache, &budget)
                .unwrap_err()
                .kind,
            RefusalKind::BunProjectionInvalid
        );
        assert!(!cache.exists(), "partial cache survived {name} refusal");
    }
}

#[test]
fn uppercase_node_modules_is_not_a_guest_project_input() {
    let scratch = ScratchDir::new("bun-guest-case");
    for (index, name) in ["NODE_MODULES", "node_moduleſ"].iter().enumerate() {
        let source = scratch.join(format!("source-{index}"));
        let ignored = source.join(name).join("hostile");
        fs::create_dir_all(&ignored).unwrap();
        fs::write(ignored.join("bun.lock"), "invalid").unwrap();
        assert!(bun::prepare_cache(
            &scratch.join("seed"),
            &source,
            &scratch.join(format!("cache-{index}"))
        )
        .unwrap()
        .is_none());
    }
}

#[test]
fn tar_metadata_is_bounded_before_automatic_long_name_processing() {
    let scratch = ScratchDir::new("bun-longname-budget");
    let mut tar = tar::Builder::new(Vec::new());
    let content = vec![b'x'; 32 * 1024 + 1];
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::GNULongName);
    header.set_mode(0o644);
    header.set_size(content.len() as u64);
    header.set_cksum();
    tar.append_data(&mut header, "././@LongLink", content.as_slice())
        .unwrap();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&tar.into_inner().unwrap()).unwrap();
    assert!(
        bun::expand_tarball(&gzip.finish().unwrap(), &scratch.join("cache"))
            .unwrap_err()
            .to_string()
            .contains("metadata limit")
    );
}
