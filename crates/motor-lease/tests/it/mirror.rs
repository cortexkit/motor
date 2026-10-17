use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use motor_lease::sha256_hex;
use serde::Deserialize;

#[derive(Deserialize)]
struct Manifest {
    files: Vec<Entry>,
}
#[derive(Deserialize)]
struct Entry {
    path: PathBuf,
    sha256: String,
}
fn inventory(root: &Path, dir: &Path, files: &mut BTreeSet<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        assert!(!entry.file_type().unwrap().is_symlink(), "mirror symlink");
        if entry.file_type().unwrap().is_dir() {
            inventory(root, &entry.path(), files);
        } else if entry.path() != root.join("digest-manifest.json") {
            files.insert(entry.path().strip_prefix(root).unwrap().to_owned());
        }
    }
}

#[test]
fn digest_manifest_covers_every_mirrored_contract_and_vector_file() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts");
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(root.join("digest-manifest.json")).unwrap()).unwrap();
    assert!(!manifest.files.is_empty());
    let mut expected = BTreeSet::new();
    for entry in manifest.files {
        assert!(entry
            .path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_))));
        assert!(
            expected.insert(entry.path.clone()),
            "duplicate manifest entry"
        );
        assert_eq!(
            sha256_hex(&std::fs::read(root.join(&entry.path)).unwrap()),
            entry.sha256,
            "digest mismatch: {}",
            entry.path.display()
        );
    }
    let mut actual = BTreeSet::new();
    inventory(&root, &root, &mut actual);
    assert_eq!(
        actual, expected,
        "mirror files must not escape digest coverage"
    );
}

#[test]
fn cloud_final_files_match_their_manifest_digests() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts");
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(root.join("digest-manifest.json")).unwrap()).unwrap();
    for path in ["cloud/job-tokens.json", "cloud/host-signature.json"] {
        let entry = manifest
            .files
            .iter()
            .find(|entry| entry.path == Path::new(path))
            .unwrap_or_else(|| panic!("missing CLOUD file in manifest: {path}"));
        assert_eq!(
            sha256_hex(&std::fs::read(root.join(path)).unwrap()),
            entry.sha256,
            "digest mismatch: {path}"
        );
    }
}
