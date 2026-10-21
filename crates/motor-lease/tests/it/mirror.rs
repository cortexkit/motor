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
    for path in [
        "cloud/runners-contract.md",
        "cloud/no-spill-lease.json",
        "cloud/no-host-fits-lease.json",
        "cloud/boat-lease.json",
        "cloud/job-tokens.json",
        "cloud/host-signature.json",
        "cloud/lease.json",
        "cloud/usage-settlement.json",
    ] {
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

#[test]
fn cloud_policy_pins_and_historical_vectors_are_byte_identical() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/cloud");
    // These published digests are independent of the editable manifest. Adding
    // a policy vector must not change the existing cloud vectors' pinned bytes.
    for (path, digest) in [
        (
            "runners-contract.md",
            "6bdd60e0c282ef767539521c9e3c7b2b1ff7c58c222796da4160bf6befb0caa1",
        ),
        (
            "no-host-fits-lease.json",
            "a9b710bf74e5e5b385cbeff3119fbeb17e58ff1a314389bf3f29edc34e1f1c2c",
        ),
        (
            "no-spill-lease.json",
            "167deadc03d9a1d8e43edf80af8a7ab840ee9641fe485c9c70a8859af0229b94",
        ),
        (
            "boat-lease.json",
            "52727d1b3e9499d5bb1a14ae4179871e642ee7addaf66939e3cd1c5a394d1f5a",
        ),
        (
            "host-signature.json",
            "9c8876854bb955aa4468ffe9cf61f68d31327bc4e13367813f8c2e89c5e1b398",
        ),
        (
            "job-tokens.json",
            "541eccc153088ef5be8755bdda9d633bed44237544acae6e365be4124eecc8bb",
        ),
        (
            "lease.json",
            "87149636bf948abed08819a52ebd69cbbff3c73159eb4995a31beea71ddfa05f",
        ),
        (
            "usage-settlement.json",
            "4f53e2eb16dd586308f31f9a9ffe986c1f214a65aa45dd973f2a3de1f63084b7",
        ),
    ] {
        assert_eq!(
            sha256_hex(&std::fs::read(root.join(path)).unwrap()),
            digest,
            "{path}"
        );
    }
}
