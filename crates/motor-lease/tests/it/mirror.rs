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
        "cloud/job-tokens-workspace.json",
        "cloud/host-signature.json",
        "cloud/host-metrics.json",
        "cloud/host-draining.json",
        "cloud/lease.json",
        "cloud/usage-settlement.json",
        "cloud/settled-reattach.json",
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
            "4676345c5f0ba8576799847bd1efee289c784fc44b2e5295e1c1e2de20ab01aa",
        ),
        (
            "host-draining.json",
            "586970bc2bfc30a8f3500a4878a8a78c8a5c95bb676a95370bfeb4063d7de722",
        ),
        (
            "host-metrics.json",
            "c1a3daf40d39f5c96ee98ad5766f18cb9af336b488b2a34d431e50405d8acf9a",
        ),
        (
            "bad-request.json",
            "262f34f1523384bafc9f4b725ddbff9053273ed93dce2f2825bee2fb4a146e39",
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
            "1d2c58991e29ff3b8c05da6a32afd22f3ab35903d15fab5ac27a6cb6006a4c61",
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
            "ea25d7cf326b9a85ff21823441720ad94f1e844c82cb7f44f868a3bbd708e96f",
        ),
        (
            "job-tokens-workspace.json",
            "2222c91ce5c80eec97c1a0c9ad9222c154d817c8ecc53801e70721020638b8be",
        ),
        (
            "lease.json",
            "baadae93c60f128cb0ec6b66f4bb9375d31de409f2961bf03170496ad45b29b2",
        ),
        (
            "usage-settlement.json",
            "4f53e2eb16dd586308f31f9a9ffe986c1f214a65aa45dd973f2a3de1f63084b7",
        ),
        (
            "settled-reattach.json",
            "8098795ac5c5377e34d93e32cc58808a54a8905f5eecb6618283aa2f0af103e7",
        ),
        (
            "account-closure.json",
            "8e3ce5eae03d06e985194f02bd817b7b45a0340e65a4bda8193969cbb3b0569e",
        ),
        (
            "long-lease.json",
            "8768f651736efacc59d532ae4ad76501a0d9b64b8d8eff880e2f32798af3487f",
        ),
        (
            "replace-lease.json",
            "98b06275ad2fc5025ce2818f429d5a922ca4fae81746dfdb3054c3141bb1fb48",
        ),
    ] {
        assert_eq!(
            sha256_hex(&std::fs::read(root.join(path)).unwrap()),
            digest,
            "{path}"
        );
    }
}
