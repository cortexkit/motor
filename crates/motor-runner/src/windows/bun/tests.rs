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
    assert!(!scratch.join(format!("sha256/{}", expected.hex)).exists());
    let quarantined = fs::read_dir(scratch.join("quarantine"))
        .unwrap()
        .collect::<io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(fs::read(quarantined[0].path()).unwrap(), b"corrupt tarball");
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
    fs::write(scratch.join(format!("sha256/{}", expected.hex)), b"bit rot").unwrap();
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
    ] {
        let mut row = row();
        row.url = url.into();
        let refusal = resolve(&config, "/repo", "m/project", &row).err().unwrap();
        assert_eq!(refusal.kind, RefusalKind::BunRegistryUnauthorized);
        assert!(!refusal.to_string().contains("secret"));
        assert_eq!(refusal.registry_origin, "https://npm.example.test");
    }
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
    assert!(guest
        .join("bun-cache/@private/win@1.0.0@@npm.example.test@@@1/package.json")
        .is_file());
    assert!(!guest
        .join("bun-cache/@private/linux@1.0.0@@npm.example.test@@@1")
        .exists());
    let unverified = guest.join("source/project/node_modules/@private/win");
    fs::create_dir_all(&unverified).unwrap();
    fs::write(
        unverified.join("package.json"),
        r#"{"name":"@private/win","version":"1.0.0"}"#,
    )
    .unwrap();
    assert_eq!(
        bun::prepare_cache(&seed, &guest.join("source"), &guest.join("stale-cache"))
            .unwrap_err()
            .kind,
        RefusalKind::BunProjectionInvalid
    );
    fs::remove_dir_all(guest.join("source/project/node_modules")).unwrap();
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
