use super::*;
use cortexkit_test_support::ScratchDir;
use rand::{RngCore, SeedableRng};

#[test]
fn dependency_bundle_hashing_memory_is_independent_of_bundle_size() {
    let root = ScratchDir::new("dependency-bundle-memory");
    let cache = root.join("cache");
    let spool = root.join("spool");
    fs::create_dir(&cache).unwrap();
    fs::create_dir(&spool).unwrap();
    git(&cache, &["init"]).unwrap();
    let mut file = File::create(cache.join("payload")).unwrap();
    let mut rng = rand::rngs::StdRng::seed_from_u64(51);
    let mut block = [0; 64 * 1024];
    for _ in 0..256 {
        rng.fill_bytes(&mut block);
        file.write_all(&block).unwrap();
    }
    drop(file);
    git(&cache, &["add", "payload"]).unwrap();
    git(
        &cache,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "payload",
        ],
    )
    .unwrap();
    let commit = git(&cache, &["rev-parse", "HEAD"]).unwrap();
    let (manifest, peak) = crate::allocation_tests::peak(|| {
        dependency_bundle(
            &cache,
            "https://example.invalid/dependency",
            &commit,
            &spool,
            0,
        )
        .unwrap()
    });
    eprintln!("dependency bundle: peak_heap_bytes={peak}");
    assert!(peak < 1024 * 1024, "bundle hashing allocated {peak} bytes");
    let bytes = fs::read(spool.join("bundle-0")).unwrap();
    assert!(bytes.len() > 16 * 1024 * 1024);
    assert_eq!(manifest.length, bytes.len() as u64);
    assert_eq!(manifest.sha256, format!("{:x}", Sha256::digest(&bytes)));
}
