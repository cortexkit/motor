use super::*;
use cortexkit_test_support::ScratchDir;

#[test]
fn replay_memory_does_not_grow_with_segment_or_torn_tail() {
    let root = ScratchDir::new("journal-replay-memory");
    let epoch = Uuid::now_v7();
    let (mut file, _) = create_segment(&root, epoch, 1).unwrap();
    for seq in 1..=20_000 {
        let payload = serde_json::to_vec(&Frame {
            epoch,
            seq,
            delta: Delta(BTreeMap::from([(
                "snapshots".into(),
                BTreeMap::from([(
                    "repository".into(),
                    Some(serde_json::json!({"commit": "a".repeat(256)})),
                )]),
            )])),
        })
        .unwrap();
        writeln!(
            file,
            "{:x}\t{}",
            Sha256::digest(&payload),
            String::from_utf8(payload.clone()).unwrap()
        )
        .unwrap();
    }
    let valid_len = file.metadata().unwrap().len();
    // A crashed writer may leave an arbitrarily long final frame.
    let block = [b'x'; 64 * 1024];
    for _ in 0..256 {
        file.write_all(&block).unwrap();
    }
    drop(file);
    let mut value = serde_json::json!({});
    let (result, peak) = crate::allocation_tests::peak(|| {
        replay(
            &root,
            &mut value,
            Position {
                version: 1,
                epoch,
                seq: 0,
                checkpoint_at_ms: 0,
            },
            true,
        )
    });
    let (position, segments) = result.unwrap();
    eprintln!("journal replay: peak_heap_bytes={peak}");
    assert!(peak < 256 * 1024, "journal replay allocated {peak} bytes");
    assert_eq!(position.seq, 20_000);
    assert_eq!(segments[0].bytes, valid_len);
    assert_eq!(fs::metadata(&segments[0].path).unwrap().len(), valid_len);
    assert_eq!(value["snapshots"]["repository"]["commit"], "a".repeat(256));
}
