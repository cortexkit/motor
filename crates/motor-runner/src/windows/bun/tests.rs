use super::*;
use cortexkit_test_support::ScratchDir;
#[cfg(unix)]
use std::sync::Arc;

fn integrity(bytes: &[u8]) -> Integrity {
    Integrity {
        algorithm: "sha256".into(),
        hex: hex_sha256(bytes),
    }
}
fn public_source() -> store::Provenance {
    store::Provenance {
        registry: "https://registry.npmjs.org/".into(),
        origin: "https://registry.npmjs.org".into(),
        scope: None,
        credentialed: false,
    }
}
fn blob(root: &Path, source: &store::Provenance, hash: &Integrity) -> PathBuf {
    source.partition(root).join(&hash.algorithm).join(&hash.hex)
}

#[test]
fn integrity_mismatch_is_refused_and_quarantined() {
    let scratch = ScratchDir::new("bun-cas-mismatch");
    let expected = integrity(b"verified compressed tarball");
    let error = verified_blob(
        scratch.path(),
        &expected,
        || Ok(b"corrupt tarball".to_vec()),
    )
    .unwrap_err();
    assert_eq!(error, RefusalKind::BunIntegrityMismatch);
    assert!(!blob(scratch.path(), &public_source(), &expected).exists());
    let quarantined = fs::read_dir(scratch.join("quarantine"))
        .unwrap()
        .collect::<io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(quarantined.len(), 1);
    let diagnostic = fs::read(quarantined[0].path()).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&diagnostic).unwrap();
    assert_eq!(value["size"], 15);
    assert_eq!(value["expected"]["hex"], expected.hex);
    assert!(!diagnostic
        .windows(15)
        .any(|part| part == b"corrupt tarball"));
    verified_blob(scratch.path(), &expected, || {
        Ok(b"second rejected body".to_vec())
    })
    .unwrap_err();
    assert_eq!(fs::read_dir(scratch.join("quarantine")).unwrap().count(), 1);
}

#[test]
fn warm_blobs_are_reverified_and_do_not_need_a_registry() {
    let scratch = ScratchDir::new("bun-cas-reuse");
    let expected = integrity(b"good tarball");
    verified_blob(scratch.path(), &expected, || Ok(b"good tarball".to_vec())).unwrap();
    assert_eq!(
        verified_blob(scratch.path(), &expected, || panic!(
            "warm cache contacted registry"
        ))
        .unwrap(),
        b"good tarball"
    );
    fs::write(
        blob(scratch.path(), &public_source(), &expected),
        b"bit rot",
    )
    .unwrap();
    assert_eq!(
        verified_blob(scratch.path(), &expected, || panic!(
            "corruption must be refused"
        ))
        .unwrap_err(),
        RefusalKind::BunIntegrityMismatch
    );
}

fn row() -> bun::LockedPackage {
    bun::LockedPackage {
        package: Package {
            placement: "alias".into(),
            name: "@private/win".into(),
            version: "1.0.0".into(),
            integrity: integrity(b"tarball"),
            windows: true,
            bins: Vec::new(),
            bin_dir: None,
        },
        url: String::new(),
    }
}

#[test]
fn private_credentials_grants_and_locked_url_policy_are_checked_before_cache_access() {
    let mut config = RegistryConfig::default();
    config.scopes.insert(
        "@private".into(),
        Scope {
            registry: "https://npm.example.test/".into(),
            credential_file: None,
            repositories: vec!["/repo".into()],
        },
    );
    let refusal = resolve(&config, "/repo", "m/project", &row())
        .err()
        .unwrap();
    assert_eq!(refusal.kind, RefusalKind::BunCredentialsUnavailable);
    assert_eq!(refusal.package, "@private");
    config.scopes.get_mut("@private").unwrap().credential_file = Some("host-only-token".into());
    assert_eq!(
        resolve(&config, "/other-repo", "m/project", &row())
            .err()
            .unwrap()
            .kind,
        RefusalKind::BunRegistryUnauthorized
    );
    for url in [
        "http://npm.example.test/p.tgz",
        "https://secret@npm.example.test/p.tgz",
        "https://npm.example.test/p.tgz?token=secret",
        "https://other.test/p.tgz",
        "https://npm.example.test/%2e%2e/p.tgz",
        "https://npm.example.test/@other/win/-/win-1.0.0.tgz",
        "https://npm.example.test/@private/other/-/other-1.0.0.tgz",
        "https://npm.example.test/@private/win/-/win-2.0.0.tgz",
    ] {
        let mut row = row();
        row.url = url.into();
        let refusal = resolve(&config, "/repo", "m/project", &row).err().unwrap();
        assert_eq!(refusal.kind, RefusalKind::BunRegistryUnauthorized);
        assert!(!refusal.to_string().contains("secret"));
        assert_eq!(refusal.registry_origin, "https://npm.example.test");
    }
}

#[test]
fn publication_provenance_prevents_public_sri_alias_hits() {
    let scratch = ScratchDir::new("bun-provenance-alias");
    let body = b"private compressed payload";
    let expected = integrity(body);
    let mut private = public_source();
    private.scope = Some("@private".into());
    private.credentialed = true;
    let mut lease = Lease::default();
    store::get(
        scratch.path(),
        &private,
        &expected,
        1024,
        &mut lease,
        || Ok(body.to_vec()),
    )
    .unwrap();
    for source in [
        public_source(),
        store::Provenance {
            scope: Some("@private".into()),
            ..public_source()
        },
    ] {
        let error = store::get(
            scratch.path(),
            &source,
            &expected,
            1024,
            &mut Lease::default(),
            || Err(RefusalKind::BunRegistryUnauthorized),
        )
        .unwrap_err();
        assert_eq!(error, RefusalKind::BunRegistryUnauthorized);
        assert!(!blob(scratch.path(), &source, &expected).exists());
    }
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(private.partition(scratch.path()).join("provenance.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["scope"], "@private");
    assert_eq!(record["credentialed"], true);
}

#[test]
fn quota_refuses_before_write_while_all_objects_are_leased() {
    let scratch = ScratchDir::new("bun-quota-active");
    let source = public_source();
    let a = integrity(b"aaaa");
    let b = integrity(b"bbbb");
    let mut running = Lease::default();
    store::get(scratch.path(), &source, &a, 4, &mut running, || {
        Ok(b"aaaa".to_vec())
    })
    .unwrap();
    assert_eq!(
        store::get(
            scratch.path(),
            &source,
            &b,
            4,
            &mut Lease::default(),
            || Ok(b"bbbb".to_vec())
        )
        .unwrap_err(),
        RefusalKind::BunCacheMissing
    );
    assert!(blob(scratch.path(), &source, &a).exists());
    assert!(!blob(scratch.path(), &source, &b).exists());
    drop(running);
    store::get(
        scratch.path(),
        &source,
        &b,
        4,
        &mut Lease::default(),
        || Ok(b"bbbb".to_vec()),
    )
    .unwrap();
    assert!(!blob(scratch.path(), &source, &a).exists());
}

#[test]
fn quota_evicts_lru_not_recent_verified_hit() {
    let scratch = ScratchDir::new("bun-quota-lru");
    let source = public_source();
    for body in [b"aaaa", b"bbbb"] {
        store::get(
            scratch.path(),
            &source,
            &integrity(body),
            8,
            &mut Lease::default(),
            || Ok(body.to_vec()),
        )
        .unwrap();
    }
    store::get(
        scratch.path(),
        &source,
        &integrity(b"aaaa"),
        8,
        &mut Lease::default(),
        || panic!("warm hit fetched"),
    )
    .unwrap();
    store::get(
        scratch.path(),
        &source,
        &integrity(b"cccc"),
        8,
        &mut Lease::default(),
        || Ok(b"cccc".to_vec()),
    )
    .unwrap();
    assert!(blob(scratch.path(), &source, &integrity(b"aaaa")).exists());
    assert!(!blob(scratch.path(), &source, &integrity(b"bbbb")).exists());
    assert!(blob(scratch.path(), &source, &integrity(b"cccc")).exists());
}

#[test]
fn registry_missing_and_unauthorized_are_indistinguishable() {
    assert_eq!(fetch_status(401), Err(RefusalKind::BunRegistryUnauthorized));
    assert_eq!(fetch_status(403), fetch_status(401));
    assert_eq!(fetch_status(404), fetch_status(401));
}

#[test]
fn uppercase_node_modules_is_not_a_host_project_input() {
    let scratch = ScratchDir::new("bun-case-project");
    for (index, name) in ["NODE_MODULES", "node_moduleſ"].iter().enumerate() {
        let root = scratch.join(index.to_string());
        let modules = root.join(name).join("hostile");
        fs::create_dir_all(&modules).unwrap();
        fs::write(modules.join("bun.lock"), "invalid").unwrap();
        let mut files = Vec::new();
        discover(&root, &mut files).unwrap();
        assert!(files.is_empty());
    }
}

#[test]
fn malformed_digest_is_refused_without_any_store_access() {
    let scratch = ScratchDir::new("bun-malformed-digest");
    let root = scratch.join("absent-store");
    let bad = Integrity {
        algorithm: "../outside".into(),
        hex: "digest".into(),
    };
    assert_eq!(
        store::get(
            &root,
            &public_source(),
            &bad,
            1024,
            &mut Lease::default(),
            || panic!("invalid digest reached fetch")
        )
        .unwrap_err(),
        RefusalKind::BunLockInvalid
    );
    assert!(!root.exists());
}

#[test]
fn local_inputs_are_lexical_and_require_discovered_manifests() {
    let checkout = PathBuf::from("unmaterialized-checkout");
    let project = checkout.join("project");
    let files = vec![checkout.join("vendor/package.json")];
    validate_local(
        "m/project",
        &project,
        &checkout,
        &["../vendor".into()],
        &files,
    )
    .unwrap();
    assert!(validate_local(
        "m/project",
        &project,
        &checkout,
        &["../absent".into()],
        &files
    )
    .is_err());
    for local in [
        r"\\evil\share",
        "C:/host",
        "",
        "/host",
        r"foo\bar",
        "../../outside",
    ] {
        assert!(validate_local("m/project", &project, &checkout, &[local.into()], &files).is_err());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn host_requires_the_memory_limited_units_parsed_plan() {
    let scratch = ScratchDir::new("bun-parser-unit");
    let config = config(scratch.path());
    let staging = scratch.join("staging");
    let checkout = staging.join("m/project");
    fs::create_dir_all(&checkout).unwrap();
    fs::write(checkout.join("package.json"), "{}").unwrap();
    fs::write(
        checkout.join("bun.lock"),
        r#"{"lockfileVersion":1,"workspaces":{"":{}},"packages":{}}"#,
    )
    .unwrap();
    assert!(
        prepare(&config, &staging, "/repo", std::slice::from_ref(&checkout))
            .unwrap_err()
            .to_string()
            .contains("BunLockInvalid")
    );
    write_parsed_locks(&staging).unwrap();
    prepare(&config, &staging, "/repo", &[checkout]).unwrap();
}

#[cfg(unix)]
fn config(root: &Path) -> crate::engine::Config {
    let mut config = crate::engine::Config::new(root.join("state"), "bun");
    config.windows = Some(Arc::new(serde_json::from_value(serde_json::json!({
        "layout_root": root, "baseline": "base", "memory_mib": 4096, "cpu_set": [0], "toolchain_version": "test"
    })).unwrap()));
    config
}

#[cfg(unix)]
#[test]
fn fake_registry_roundtrip_keeps_credentials_off_the_archive_and_guest_manifest() {
    use base64::Engine;
    use std::{io::Cursor, net::TcpListener, os::unix::fs::PermissionsExt};
    let scratch = ScratchDir::new("bun-fake-registry");
    let mut archives = BTreeMap::new();
    for platform in ["win", "linux"] {
        let mut tar = tar::Builder::new(Vec::new());
        let json = format!(r#"{{"name":"@private/{platform}","version":"1.0.0"}}"#);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(json.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, "package/package.json", json.as_bytes())
            .unwrap();
        let path = scratch.join(format!("{platform}.tar"));
        fs::write(&path, tar.into_inner().unwrap()).unwrap();
        let compressed = std::process::Command::new("gzip")
            .args(["-c"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(compressed.status.success());
        archives.insert(platform, compressed.stdout);
    }
    let lock = serde_json::json!({"lockfileVersion": 1, "workspaces": {"": {"optionalDependencies": {"@private/win": "1.0.0", "@private/linux": "1.0.0"}}}, "packages": {
        "@private/win": ["@private/win@1.0.0", "", {"os": "win32", "cpu": "x64"}, format!("sha256-{}", base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&archives["win"])))],
        "@private/linux": ["@private/linux@1.0.0", "", {"os": "linux"}, format!("sha256-{}", base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&archives["linux"])))],
    }}).to_string();
    let mut config = config(scratch.path());
    let secrets = config.state_dir().join("bun-credentials");
    fs::create_dir_all(&secrets).unwrap();
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700)).unwrap();
    let credential_path = secrets.join("read-packages-token");
    let secret = "host-only-test-bearer-never-in-guest";
    fs::write(&credential_path, secret).unwrap();
    fs::set_permissions(&credential_path, fs::Permissions::from_mode(0o600)).unwrap();
    Arc::make_mut(config.windows.as_mut().unwrap())
        .bun
        .scopes
        .insert(
            "@private".into(),
            Scope {
                registry: "https://npm.example.test/".into(),
                credential_file: Some(credential_path.clone()),
                repositories: vec!["/repo".into()],
            },
        );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request
                .to_ascii_lowercase()
                .contains(&format!("authorization: bearer {secret}")));
            let platform = if request.starts_with("GET /@private/win/") {
                "win"
            } else {
                "linux"
            };
            let body = &archives[platform];
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
    });
    let staging = scratch.join("staging");
    let checkout = staging.join("m/project");
    fs::create_dir_all(&checkout).unwrap();
    fs::write(checkout.join("package.json"), "{}").unwrap();
    fs::write(checkout.join("bun.lock"), &lock).unwrap();
    prepare_with_fetch(
        &config,
        &staging,
        "/repo",
        std::slice::from_ref(&checkout),
        |target| {
            assert_eq!(target.url.host_str(), Some("npm.example.test"));
            let token = credential(target.credential_file.unwrap()).unwrap();
            let response = reqwest::blocking::Client::new()
                .get(format!("http://{address}{}", target.url.path()))
                .bearer_auth(token)
                .send()
                .unwrap();
            Ok(response.bytes().unwrap().to_vec())
        },
    )
    .unwrap();
    server.join().unwrap();
    super::super::prep::build_tar(&staging).unwrap();
    let archive = fs::read(staging.join("tree.tar")).unwrap();
    assert!(!archive
        .windows(secret.len())
        .any(|part| part == secret.as_bytes()));
    assert!(!archive
        .windows(credential_path.to_string_lossy().len())
        .any(|part| part == credential_path.to_string_lossy().as_bytes()));
    let manifest_bytes = fs::read(staging.join("bun-seed/manifest.json")).unwrap();
    assert!(!manifest_bytes
        .windows(secret.len())
        .any(|part| part == secret.as_bytes()));
    let guest = scratch.join("guest");
    fs::create_dir(&guest).unwrap();
    let seed = guest.join("seed");
    fs::create_dir(&seed).unwrap();
    bun::unpack_seed(&mut Cursor::new(&archive), &seed).unwrap();
    motor_winagent::unpack_tree(
        &mut Cursor::new(&archive),
        &guest.join("source"),
        &guest.join("cargo-home"),
    )
    .unwrap();
    let manifest = bun::prepare_cache(&seed, &guest.join("source"), &guest.join("bun-cache"))
        .unwrap()
        .unwrap();
    assert_eq!(manifest.projects[0].packages.len(), 2);
    let linux = manifest.projects[0]
        .packages
        .iter()
        .find(|p| p.name == "@private/linux")
        .unwrap();
    assert!(
        !seed.join(linux.integrity.blob_path()).exists(),
        "foreign-platform bytes crossed into the guest"
    );
    assert!(guest
        .join("bun-cache/@private/win@1.0.0@@npm.example.test@@@1/package.json")
        .is_file());
    assert!(!guest
        .join("bun-cache/@private/linux@1.0.0@@npm.example.test@@@1")
        .exists());
    for (index, name) in ["NODE_MODULES", "node_moduleſ"].iter().enumerate() {
        let modules = guest.join("source/project").join(name);
        let unverified = modules.join("@private/win");
        fs::create_dir_all(&unverified).unwrap();
        fs::write(
            unverified.join("package.json"),
            r#"{"name":"@private/win","version":"1.0.0"}"#,
        )
        .unwrap();
        assert_eq!(
            bun::prepare_cache(
                &seed,
                &guest.join("source"),
                &guest.join(format!("stale-cache-{index}"))
            )
            .unwrap_err()
            .kind,
            RefusalKind::BunProjectionInvalid
        );
        fs::remove_dir_all(modules).unwrap();
    }
    let win = manifest.projects[0]
        .packages
        .iter()
        .find(|p| p.name == "@private/win")
        .unwrap();
    fs::write(seed.join(win.integrity.blob_path()), b"changed in transit").unwrap();
    let refusal =
        bun::prepare_cache(&seed, &guest.join("source"), &guest.join("corrupt-cache")).unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::BunIntegrityMismatch);
    assert_eq!(refusal.package, "@private/win");
    assert_eq!(refusal.registry_origin, "https://npm.example.test");
    let warm = scratch.join("warm");
    let warm_checkout = warm.join("m/project");
    fs::create_dir_all(&warm_checkout).unwrap();
    fs::write(warm_checkout.join("package.json"), "{}").unwrap();
    fs::write(warm_checkout.join("bun.lock"), lock).unwrap();
    prepare_with_fetch(&config, &warm, "/repo", &[warm_checkout], |_| {
        panic!("verified warm cache must not fetch")
    })
    .unwrap();
}
