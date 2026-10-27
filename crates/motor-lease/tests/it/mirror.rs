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
        "cloud/bad-request.json",
        "cloud/no-spill-lease.json",
        "cloud/no-host-fits-lease.json",
        "cloud/explicit-only-leases.json",
        "cloud/boat-lease.json",
        "cloud/job-tokens.json",
        "cloud/host-signature.json",
        "cloud/lease.json",
        "cloud/usage-settlement.json",
        "cloud/settled-reattach.json",
        "cloud/shadow-lease.json",
        "cloud/account-closure.json",
        "cloud/long-lease.json",
        "cloud/replace-lease.json",
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
    // These digests are independent of the editable manifest, so updating a
    // canonical contract or vector requires an explicit change to both pins.
    for (path, digest) in [
        (
            "runners-contract.md",
            "520a1817354d156850de324367470e90042170c238ced37318662970d5a3aead",
        ),
        (
            "bad-request.json",
            "4f2d775a85a89d7024fd65186162b53bed696ccf0ad02ce839efc5949ffd1fbc",
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
            "explicit-only-leases.json",
            "93136da9d2703acee05e80889ed18c1aaa1d0dd1623848548d08c29b5f673708",
        ),
        (
            "boat-lease.json",
            "a8bdda4fa90dff6607e534efabfa1a17c71b52377af91b714a7a50b9fd4f83bb",
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
            "aa3996947d5da23807cd32813be57c673610c23ed704d92bf95b24db8ee14d81",
        ),
        (
            "usage-settlement.json",
            "4f53e2eb16dd586308f31f9a9ffe986c1f214a65aa45dd973f2a3de1f63084b7",
        ),
        (
            "settled-reattach.json",
            "59308e456afcde26a9fa9973bd9b243a67f8f8cb3886dc8f20c344c597176b16",
        ),
        (
            "shadow-lease.json",
            "cf0f4481a403e2ab3ab2e7cf65a3375bf70131e4e2e8ad1786e6541c9cf33574",
        ),
        (
            "account-closure.json",
            "8e3ce5eae03d06e985194f02bd817b7b45a0340e65a4bda8193969cbb3b0569e",
        ),
        (
            "long-lease.json",
            "a0fb260259ccf90b00ecf8d08af5b06aa649564ba0f898ebac8987c0eaab2b4c",
        ),
        (
            "replace-lease.json",
            "96c5abfae5898ac7756847152f6e7f9aca6791d94880b29b5758ff1d5ce077d6",
        ),
    ] {
        assert_eq!(
            sha256_hex(&std::fs::read(root.join(path)).unwrap()),
            digest,
            "{path}"
        );
    }
}
