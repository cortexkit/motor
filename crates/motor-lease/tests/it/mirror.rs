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
        "cloud/stated-memory-lease.json",
        "cloud/explicit-only-leases.json",
        "cloud/boat-lease.json",
        "cloud/job-tokens.json",
        "cloud/job-tokens-workspace.json",
        "cloud/mem-burst.json",
        "cloud/host-signature.json",
        "cloud/host-metrics.json",
        "cloud/host-draining.json",
        "cloud/tenant-disk.json",
        "cloud/lease.json",
        "cloud/usage-settlement.json",
        "cloud/settled-reattach.json",
        "cloud/account-closure.json",
        "cloud/long-lease.json",
        "cloud/replace-lease.json",
        "cloud/host-revoke-fence-off.json",
        "cloud/host-revoke-fence-on.json",
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
            "bc8bb42107fb572bfd9e1f858cdb6f3593533e3bd79194e38de144b8ab61974f",
        ),
        (
            "mem-burst.json",
            "ef702927a7d7515086dee4bdfe7eb1f1fe91c97cb329cd244814724269b0933b",
        ),
        (
            "host-revoke-fence-off.json",
            "86bb9fd961fc86a75784bbe03fef6bbcfd4eb0bd3c9b07fb6bd845f92aeb6df9",
        ),
        (
            "host-revoke-fence-on.json",
            "989b55f465db090e1da1e5e517a0d13622f4c3f13336dcbfcb3a66872195827b",
        ),
        (
            "host-draining.json",
            "849a17ec763c5b686cd3c99d48d55a577d677cb6c0a3a024184d5024b4902c25",
        ),
        (
            "tenant-disk.json",
            "5843daf2cb7a70cd4be2d34c70c09347f8b08eb922df6a2f2ee390599cfc4c07",
        ),
        (
            "host-metrics.json",
            "51c927ce42be0b8d79907ecacf999d28430988f836ce6474c40a0a24f0729fc3",
        ),
        (
            "bad-request.json",
            "262f34f1523384bafc9f4b725ddbff9053273ed93dce2f2825bee2fb4a146e39",
        ),
        (
            "no-host-fits-lease.json",
            "99bb8a43fd54ae8a709670b13df9f2e1b5a1fde0be2d9ae5b526b7d13ff46a26",
        ),
        (
            "stated-memory-lease.json",
            "c82ae45c2974f948605eab5244154be67a84115056c62eda66667f96d57bc288",
        ),
        (
            "no-spill-lease.json",
            "167deadc03d9a1d8e43edf80af8a7ab840ee9641fe485c9c70a8859af0229b94",
        ),
        (
            "explicit-only-leases.json",
            "42cbd8136daef1be9428296de14cfcbd0d077f029a7a41ffd3e6668c46a83629",
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
            "417efcb8af6bbb6df19e94eb2672878dc1af319741a40c7a93dfeec5f39ea93e",
        ),
        (
            "job-tokens-workspace.json",
            "a8e6ad7afd8fa79c0dc3efdca81068d4ad72b7a885866dfca6df54597e92ce39",
        ),
        (
            "lease.json",
            "7b66236303bc2d541f1dc5e0f7ec6dc50b54ad7258840858123c13e2170aa001",
        ),
        (
            "usage-settlement.json",
            "5af0bad18551107865747a1bdbdb70f4b0556effe7311ff79687b36b034ee54b",
        ),
        (
            "settled-reattach.json",
            "1d6a35763c9fd66da3e2383fdcce0ec5903ec19fe27f2c60ff917268bf6ec55e",
        ),
        (
            "account-closure.json",
            "8e3ce5eae03d06e985194f02bd817b7b45a0340e65a4bda8193969cbb3b0569e",
        ),
        (
            "long-lease.json",
            "71feb561ca70e2989b6b2a1cdb149b4d99d5e4ba5583d571a2f45a5032bec73a",
        ),
        (
            "replace-lease.json",
            "f0dd550e59289fb02e172d61ad463b83f72ccb4edaffccf1c3a4e8266fc88a8b",
        ),
    ] {
        assert_eq!(
            sha256_hex(&std::fs::read(root.join(path)).unwrap()),
            digest,
            "{path}"
        );
    }
}
