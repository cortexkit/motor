use cortexkit_test_support::ScratchDir;
use motor_winagent::bun::*;
use std::collections::BTreeMap;
use std::io::Write;

#[test]
fn parses_real_locks_without_host_platform_filtering() {
    let rows = read_lock(
        "m/project",
        include_str!("../../../../../script/runner/network-fixture/bun.lock"),
    )
    .unwrap();
    assert_eq!(rows.len(), 413);
    assert!(rows
        .iter()
        .any(|p| p.package.name == "@esbuild/linux-loong64" && !p.package.windows));
    assert!(rows
        .iter()
        .any(|p| p.package.name == "@esbuild/win32-x64" && p.package.windows));
    assert!(read_lock(
        "m/project",
        include_str!("../../../test-fixtures/javascript/bun/bun.lock")
    )
    .unwrap()
    .is_empty());
}

pub(super) fn package() -> Package {
    Package {
        placement: "@private/win".into(),
        name: "@private/win".into(),
        version: "1.0.0".into(),
        integrity: Integrity::parse("sha256-LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=").unwrap(),
        windows: true,
        bins: Vec::new(),
        bin_dir: None,
    }
}

#[test]
fn refuses_missing_integrity_and_remote_nonregistry_sources() {
    for (resolution, tail, kind) in [
        ("p@1.0.0", ",\"\",{}", RefusalKind::BunIntegrityMissing),
        (
            "p@git+https://example.org/p",
            "",
            RefusalKind::BunSourceUnsupported,
        ),
        (
            "p@https://example.org/p.tgz",
            ",{}",
            RefusalKind::BunSourceUnsupported,
        ),
        (
            "p@1.0.0",
            ",\"\",{},\"sha1-fake\"",
            RefusalKind::BunIntegrityMissing,
        ),
    ] {
        let lock = format!(
            "{{\"lockfileVersion\":1,\"packages\":{{\"alias\":[\"{resolution}\"{tail}]}}}}"
        );
        assert_eq!(read_lock("m/p", &lock).unwrap_err().kind, kind);
    }
    assert!(read_lock("m/p", "{\"lockfileVersion\":1,\"packages\":[],}").is_err());
    assert!(read_lock("m/p", "{\"lockfileVersion\":3,\"packages\":{},}").is_err());
    assert!(text_json(r#"{"packages":{"alias":[],"alias":[]}}"#).is_err());
    let json = text_json(r#"{"x":"a,}b\\\"", "y":[1,],}"#).unwrap();
    assert_eq!(json["y"][0], 1);
}

#[test]
fn integrity_rejects_corrupt_compressed_bytes() {
    let integrity = package().integrity;
    assert!(integrity.verify(b"hello"));
    assert!(!integrity.verify(b"tampered"));
    let sha512 = Integrity { algorithm: "sha512".into(), hex: "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca72323c3d99ba5c11d7c7acc6e14b8c5da0c4663475c2e5c3adef46f73bcdec043".into() };
    assert!(sha512.verify(b"hello"));
    assert!(!sha512.verify(b"tampered"));
    assert!(Integrity::parse("sha512-AA==").is_none());
    assert!(Integrity::parse("sha256-../outside").is_none());
}

fn gzip_tar(entries: &[(&str, tar::EntryType)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, kind) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*kind);
        header.set_mode(0o777);
        let bytes = if kind.is_file() {
            b"{}".as_slice()
        } else {
            b"".as_slice()
        };
        header.set_size(bytes.len() as u64);
        // Raw path bytes permit traversal fixtures that tar's safe builder rejects.
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        if kind.is_symlink() || kind.is_hard_link() {
            header.set_link_name("package.json").unwrap();
        }
        header.set_cksum();
        builder.append(&header, bytes).unwrap();
    }
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&builder.into_inner().unwrap()).unwrap();
    gzip.finish().unwrap()
}

#[test]
fn expansion_refuses_traversal_links_reserved_names_and_case_collisions() {
    let scratch = ScratchDir::new("bun-expansion-hostile");
    for (i, entries) in [
        vec![
            ("package/package.json", tar::EntryType::Regular),
            ("package/../escape", tar::EntryType::Regular),
        ],
        vec![
            ("package/package.json", tar::EntryType::Regular),
            ("package/link", tar::EntryType::Symlink),
        ],
        vec![
            ("package/package.json", tar::EntryType::Regular),
            ("package/link", tar::EntryType::Link),
        ],
        vec![
            ("package/package.json", tar::EntryType::Regular),
            ("package/A/x", tar::EntryType::Regular),
            ("package/a/y", tar::EntryType::Regular),
        ],
        vec![
            ("package/package.json", tar::EntryType::Regular),
            ("package/NUL.txt", tar::EntryType::Regular),
        ],
        vec![
            ("package/package.json", tar::EntryType::Regular),
            ("package/a", tar::EntryType::Regular),
            ("package/a/x", tar::EntryType::Regular),
        ],
        vec![
            ("package/package.json", tar::EntryType::Regular),
            ("package/C:/escape", tar::EntryType::Regular),
        ],
    ]
    .iter()
    .enumerate()
    {
        let dest = scratch.join(format!("bad-{i}"));
        assert!(expand_tarball(&gzip_tar(entries), &dest).is_err());
        assert!(!dest.exists(), "preflight wrote files for {entries:?}");
    }
    let dest = scratch.join("good");
    expand_tarball(
        &gzip_tar(&[
            ("arbitrary-prefix/", tar::EntryType::Directory),
            ("arbitrary-prefix/package.json", tar::EntryType::Regular),
        ]),
        &dest,
    )
    .unwrap();
    assert_eq!(std::fs::read(dest.join("package.json")).unwrap(), b"{}");
}

#[test]
fn missing_optional_windows_package_and_bin_are_named_refusals() {
    let scratch = ScratchDir::new("bun-missing-optional");
    let project = Project {
        path: "m/project".into(),
        lock_sha256: String::new(),
        manifest_sha256: String::new(),
        name: "root".into(),
        version: "1.0.0".into(),
        packages: vec![package()],
    };
    let refusal = verify_installed(&project, scratch.path()).unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::BunOfflineInstallFailed);
    assert_eq!(refusal.package, "@private/win");
    assert_eq!(refusal.version, "1.0.0");
    let installed = scratch.join("node_modules/@private/win");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::write(
        installed.join("package.json"),
        r#"{"name":"@private/win","version":"1.0.0","bin":{"tool":"tool.exe"}}"#,
    )
    .unwrap();
    assert_eq!(
        verify_installed(&project, scratch.path())
            .unwrap_err()
            .package,
        "@private/win"
    );
    std::fs::write(installed.join("tool.exe"), b"binary").unwrap();
    verify_installed(&project, scratch.path()).unwrap();
}

#[test]
fn keys_follow_bun_144_registry_and_prerelease_formats() {
    let mut package = package();
    assert_eq!(
        cache_key(&package, &BTreeMap::new()).unwrap(),
        "@private/win@1.0.0@@@1"
    );
    let registries = BTreeMap::from([("@private".into(), "https://registry.npmjs.org/".into())]);
    assert_eq!(
        cache_key(&package, &registries).unwrap(),
        "@private/win@1.0.0@@registry.npmjs.org@@@1"
    );
    package.version = "1.0.0-rc.1+build".into();
    assert_eq!(
        cache_key(&package, &BTreeMap::new()).unwrap(),
        "@private/win@1.0.0-6b6f27d79dec3201+AE29F05561765D46@@@1"
    );
    package.version = "1.0.0rc.1+build".into();
    assert_eq!(
        cache_key(&package, &BTreeMap::new()).unwrap(),
        "@private/win@1.0.0-6b6f27d79dec3201+AE29F05561765D46@@@1"
    );
}

#[test]
fn bun_failure_is_only_legal_before_tree_received_and_version_four_is_named() {
    use motor_winagent::{Command, Frame, Peer, Protocol, State};
    let mut protocol = Protocol::new("nonce").unwrap();
    let error = protocol
        .accept(
            Peer::Agent,
            &Frame::Hello {
                version: 4,
                nonce: "nonce".into(),
            },
        )
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("unsupported agent protocol version 4"));
    let mut protocol = Protocol::new("nonce").unwrap();
    protocol
        .accept(
            Peer::Agent,
            &Frame::Hello {
                version: 5,
                nonce: "nonce".into(),
            },
        )
        .unwrap();
    protocol
        .accept(
            Peer::Host,
            &Frame::TreeEnd {
                command: Command {
                    command: "echo should-not-run".into(),
                    cwd: "/project".into(),
                    workspace: "/project".into(),
                    env: BTreeMap::new(),
                    source_commit: "commit".into(),
                },
            },
        )
        .unwrap();
    protocol
        .accept(
            Peer::Agent,
            &Frame::BunPreparationFailed {
                refusal: refuse(
                    RefusalKind::BunOfflineInstallFailed,
                    "m/project",
                    "@private/win",
                    "1.0.0",
                ),
            },
        )
        .unwrap();
    assert_eq!(protocol.state(), State::Failed);
    assert!(protocol.accept(Peer::Host, &Frame::Start {}).is_err());
}

#[test]
fn archive_allows_only_the_versioned_bun_seed_members_and_requires_a_manifest() {
    use std::io::Cursor;
    for name in [
        "another-seed/file",
        "bun-seed/token",
        "bun-seed/blobs/md5/deadbeef",
        "bun-seed/blobs/sha256/../escape",
        "bun-seed/manifest.json",
    ] {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_mode(0o644);
        header.set_cksum();
        if name.contains("..") {
            assert!(motor_winagent::validate_archive_path(name).is_err());
            continue;
        }
        builder
            .append_data(&mut header, name, b"{}".as_slice())
            .unwrap();
        let archive = builder.into_inner().unwrap();
        if name == "bun-seed/manifest.json" {
            motor_winagent::validate_tree(&mut Cursor::new(archive)).unwrap();
        } else {
            assert!(
                motor_winagent::validate_tree(&mut Cursor::new(archive)).is_err(),
                "accepted {name}"
            );
        }
    }
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(2);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(
            &mut header,
            format!("bun-seed/blobs/sha256/{}", "a".repeat(64)),
            b"{}".as_slice(),
        )
        .unwrap();
    assert!(
        motor_winagent::validate_tree(&mut Cursor::new(builder.into_inner().unwrap())).is_err()
    );
}

#[test]
fn missing_seed_is_refused_even_when_a_host_omits_the_bun_namespace() {
    let scratch = ScratchDir::new("bun-seed-missing");
    let source = scratch.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("bun.lock"), "{}").unwrap();
    assert_eq!(
        prepare_cache(&scratch.join("seed"), &source, &scratch.join("cache"))
            .unwrap_err()
            .kind,
        RefusalKind::BunCacheMissing
    );
}
