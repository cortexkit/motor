use super::*;
use crate::state::jobs::storage::{REPLAY_BYTES, REPLAY_RESERVATION_BYTES, RETURN_BYTES};

fn manifest(seq: u64, sizes: &[u64]) -> Vec<u8> {
    let files: Vec<_> = sizes
        .iter()
        .enumerate()
        .map(|(i, size)| {
            motor_protocol::ReturnManifestEntry::returned(
                format!("/out/{i}"),
                *size,
                "0".repeat(64),
            )
        })
        .collect();
    let mut value = json!({"type":"return_manifest","job_id":JOB,"seq":seq,"cap_bytes":RETURN_BYTES,"total_bytes":sizes.iter().sum::<u64>()});
    value["files"] = serde_json::to_value(files).unwrap();
    frame(value)
}
fn chunk(seq: u64, path: u64, bytes: Vec<u8>) -> Vec<u8> {
    frame(
        serde_json::to_value(motor_protocol::Frame::ReturnChunk(
            motor_protocol::ReturnChunk::new(
                uuid::Uuid::parse_str(JOB).unwrap(),
                seq,
                path as u32,
                0,
                motor_protocol::BytePayload(bytes),
            ),
        ))
        .unwrap(),
    )
}
fn drain(store: &Store, mut cursor: u64, mut from: u64) -> Vec<Value> {
    let mut frames = Vec::new();
    while let Some((next, bytes)) = store.cursor_frame(cursor, from).unwrap() {
        let value = decode(&bytes).unwrap();
        if let Some(seq) = value["seq"].as_u64() {
            from = seq + 1;
        }
        cursor = next;
        frames.push(value);
    }
    frames
}

#[test]
fn far_over_replay_bytes_maximum_returns_restart_and_expiry_stay_reserved() {
    let root = scratch();
    let mut store = Store::open(root.path(), JOB).unwrap();
    store
        .append(&frame(json!({"type":"started","seq":0})))
        .unwrap();
    let mut cursor = 0;
    for seq in 1..=256 {
        store
            .append(&frame(
                json!({"type":"output","seq":seq,"stream":"stdout","bytes":"x".repeat(700_000)}),
            ))
            .unwrap();
        if seq == 10 {
            cursor = store.next_id();
        }
        if seq == 128 {
            let id = store.next_id();
            assert!(store.index.truncated_before_seq.is_some());
            drop(store);
            store = Store::open(root.path(), JOB).unwrap();
            assert_eq!(store.next_id(), id);
        }
        assert!(allocated_bytes(root.path(), JOB).unwrap() <= REPLAY_RESERVATION_BYTES);
    }
    let retained: Vec<_> = store
        .index
        .entries
        .iter()
        .filter(|entry| entry.kind.as_deref() == Some("output"))
        .map(|entry| entry.seq.unwrap())
        .collect();
    let earliest = retained[0];
    assert!(earliest > 10);
    assert_eq!(retained, (earliest..=256).collect::<Vec<_>>());
    let persisted: Value =
        serde_json::from_reader(open(&store.directory, "index.json", libc::O_RDONLY).unwrap())
            .unwrap();
    assert_eq!(persisted["truncated_before_seq"], earliest);
    let segments: u64 = data_files(&store.directory)
        .unwrap()
        .into_iter()
        .filter(|name| name.starts_with("segment-"))
        .map(|name| {
            open(&store.directory, &name, libc::O_RDONLY)
                .unwrap()
                .metadata()
                .unwrap()
                .len()
        })
        .sum();
    assert!(segments <= REPLAY_BYTES);
    for i in 0..256 {
        assert!(store
            .append(&chunk(257 + i, 0, vec![b'r'; 512 * 1024]))
            .unwrap());
        if i % 16 == 0 {
            let allocated = allocated_bytes(root.path(), JOB).unwrap();
            assert!(
                allocated <= REPLAY_RESERVATION_BYTES,
                "chunk {i}: allocated {allocated}, reservation {REPLAY_RESERVATION_BYTES}"
            );
        }
    }
    assert_eq!(store.index.return_bytes, Some(RETURN_BYTES));
    store.append(&manifest(513, &[RETURN_BYTES])).unwrap();
    store
        .append(&frame(json!({"type":"terminal","job_id":JOB})))
        .unwrap();
    for frames in [drain(&store, 0, 0), drain(&store, cursor, 11)] {
        let outputs: Vec<_> = frames
            .iter()
            .filter(|frame| frame["type"] == "output")
            .collect();
        assert_eq!(outputs.len(), retained.len());
        assert_eq!(outputs[0]["truncated_before_seq"], earliest);
        for (output, seq) in outputs.iter().zip(&retained) {
            assert_eq!(output["seq"], *seq);
            assert_eq!(output["bytes"].as_str().unwrap(), "x".repeat(700_000));
        }
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["type"] == "terminal")
                .count(),
            1
        );
    }
    let before = allocated_bytes(root.path(), JOB).unwrap();
    drop(store);
    let mut store = Store::open(root.path(), JOB).unwrap();
    let replay = drain(&store, 0, 0);
    assert_eq!(replay[1]["truncated_before_seq"], earliest);
    assert_eq!(replay.last().unwrap()["type"], "terminal");
    store.expire_chunks().unwrap();
    assert!(allocated_bytes(root.path(), JOB).unwrap() + RETURN_BYTES <= before + 4096);
    let replay = drain(&store, 0, 0);
    assert!(!replay.iter().any(|frame| frame["type"] == "return_chunk"));
    let manifest: motor_protocol::ReturnManifest = serde_json::from_value(
        replay
            .iter()
            .find(|f| f["type"] == "return_manifest")
            .unwrap()
            .clone(),
    )
    .unwrap();
    assert!(matches!(
        manifest.files[0].outcome,
        motor_protocol::ReturnManifestOutcome::Failed {
            cause: motor_protocol::ReturnPathsFailureCause::Expired,
            ..
        }
    ));
    assert_eq!(manifest.total_bytes, 0);
    drop(store);
    let store = Store::open(root.path(), JOB).unwrap();
    assert_eq!(
        drain(&store, 0, 0)
            .iter()
            .filter(|f| f["type"] == "terminal")
            .count(),
        1
    );
    assert!(allocated_bytes(root.path(), JOB).unwrap() <= REPLAY_RESERVATION_BYTES - RETURN_BYTES);
}

#[test]
fn excessive_return_frames_fail_only_excess_paths_and_keep_terminal() {
    let root = scratch();
    let mut store = Store::open(root.path(), JOB).unwrap();
    store.set_return_limit(5).unwrap();
    assert!(store.append(&chunk(1, 0, b"abc".to_vec())).unwrap());
    assert!(store.append(&chunk(2, 1, b"de".to_vec())).unwrap());
    assert!(
        store.cursor_frame(0, 0).unwrap().is_none(),
        "partial files must stay private until their manifest is committed"
    );
    assert!(!store.append(&chunk(3, 1, b"excess".to_vec())).unwrap());
    assert!(!store.append(&chunk(4, 1, b"more".to_vec())).unwrap());
    use sha2::{Digest, Sha256};
    let mut m = decode(&manifest(5, &[3, 12])).unwrap();
    m["files"] = serde_json::to_value(vec![
        motor_protocol::ReturnManifestEntry::returned(
            "/out/0",
            3,
            format!("{:x}", Sha256::digest(b"abc")),
        )
        .with_chunks(1),
        motor_protocol::ReturnManifestEntry::returned("/out/1", 12, "0".repeat(64)).with_chunks(3),
    ])
    .unwrap();
    store.append(&frame(m)).unwrap();
    store.append(&frame(json!({"type":"terminal"}))).unwrap();
    drop(store);
    let mut store = Store::open(root.path(), JOB).unwrap();
    let frames = drain(&store, 0, 0);
    assert_eq!(
        frames
            .iter()
            .filter(|f| f["type"] == "return_chunk")
            .count(),
        1
    );
    let m: motor_protocol::ReturnManifest = serde_json::from_value(frames[1].clone()).unwrap();
    assert!(matches!(
        m.files[0].outcome,
        motor_protocol::ReturnManifestOutcome::Returned { .. }
    ));
    assert!(matches!(
        m.files[1].outcome,
        motor_protocol::ReturnManifestOutcome::Failed {
            cause: motor_protocol::ReturnPathsFailureCause::OverCap,
            ..
        }
    ));
    assert_eq!(m.total_bytes, 3);
    let paths = vec!["/out/0".to_owned(), "/out/1".to_owned()];
    let mut assembler = motor_protocol::ReturnAssembler::new(&paths, RETURN_BYTES);
    for value in &frames[..1] {
        let chunk: motor_protocol::ReturnChunk = serde_json::from_value(value.clone()).unwrap();
        assembler.accept_chunk(&chunk).unwrap();
    }
    assembler
        .finish(&m)
        .expect("caller must be able to consume the failed return manifest");
    assert_eq!(frames.iter().filter(|f| f["type"] == "terminal").count(), 1);
    assert_eq!(store.last_seq(), Some(5));
    store.expire_chunks().unwrap();
    let frames = drain(&store, 0, 0);
    let manifest: motor_protocol::ReturnManifest =
        serde_json::from_value(frames[0].clone()).unwrap();
    motor_protocol::ReturnAssembler::new(&paths, RETURN_BYTES)
        .finish_expired_replay(&manifest, 0)
        .expect("expiry must account for both accepted and refused chunk sequences");
}

#[test]
fn v1_index_loads_conservatively_and_preserves_raw_frames() {
    let root = scratch();
    let store = Store::open(root.path(), JOB).unwrap();
    let first = frame(json!({"type":"started","seq":0}));
    let second = frame(json!({"type":"output","seq":1,"bytes":"old"}));
    let mut file = open(
        &store.directory,
        "segment-0000000000000000",
        libc::O_RDWR | libc::O_CREAT,
    )
    .unwrap();
    file.write_all(&[first.clone(), second.clone()].concat())
        .unwrap();
    file.sync_all().unwrap();
    let index = json!({"version":1,"last_seq":1,"terminal":false,"entries":[
        {"seq":0,"pieces":[{"chunk":false,"number":0,"offset":0,"len":first.len()}]},
        {"seq":1,"pieces":[{"chunk":false,"number":0,"offset":first.len(),"len":second.len()}]}
    ]});
    let mut file = open(&store.directory, "index.json", libc::O_RDWR).unwrap();
    file.set_len(0).unwrap();
    file.write_all(&serde_json::to_vec(&index).unwrap())
        .unwrap();
    file.sync_all().unwrap();
    drop(store);
    let mut store = Store::open(root.path(), JOB).unwrap();
    assert_eq!(store.index.return_limit, None);
    assert_eq!(store.index.chunks_expired, None);
    assert_eq!(store.frame(0).unwrap(), Some(first.clone()));
    assert_eq!(store.frame(1).unwrap(), Some(second));
    assert!(store.append(&chunk(2, 0, b"allowed".to_vec())).unwrap());
    assert_eq!(store.next_id(), 3);
    assert_eq!(store.frame(0).unwrap(), Some(first));
}

#[test]
fn retention_reclaims_quarantined_frames_without_parsing_the_broken_index() {
    let root = scratch();
    let mut store = Store::open(root.path(), JOB).unwrap();
    store.append(&chunk(1, 0, vec![1; 512 * 1024])).unwrap();
    drop(store);
    std::fs::write(
        root.path().join("replay").join(JOB).join("index.json"),
        b"broken index",
    )
    .unwrap();
    remove(root.path(), JOB).unwrap();
    assert_eq!(Store::open(root.path(), JOB).unwrap().len(), 0);
    assert!(allocated_bytes(root.path(), JOB).unwrap() <= 4096);
}

#[test]
fn recovery_requires_recorded_truncation_ranges_for_unused_output_prefixes() {
    let root = scratch();
    let mut store = Store::open(root.path(), JOB).unwrap();
    let first = frame(json!({"type":"output","seq":1,"bytes":"first"}));
    let second = frame(json!({"type":"output","seq":2,"bytes":"second"}));
    store.append(&first).unwrap();
    store.append(&second).unwrap();
    let mut index = store.index.clone();
    let removed = index.entries.remove(0);
    store.commit(&index).unwrap();
    assert!(Store::open(root.path(), JOB)
        .err()
        .unwrap()
        .to_string()
        .contains("unexplained replay gap"));
    index.truncated_before_seq = Some(2);
    index.dropped = removed
        .pieces
        .into_iter()
        .map(|piece| Dropped {
            piece,
            seq: Some(1),
            return_path: None,
        })
        .collect();
    store.commit(&index).unwrap();
    drop(store);
    let store = Store::open(root.path(), JOB).unwrap();
    assert_eq!(store.frame(0).unwrap(), Some(second));
    let replay = drain(&store, 0, 0);
    assert_eq!(replay[0]["truncated_before_seq"], 2);
}

#[test]
fn recovery_requires_a_refused_tail_to_explain_a_watermark_jump() {
    let root = scratch();
    let mut store = Store::open(root.path(), JOB).unwrap();
    store
        .append(&frame(json!({"type":"output","seq":1,"bytes":"kept"})))
        .unwrap();
    let mut damaged = store.index.clone();
    damaged.last_seq = Some(3);
    store.commit(&damaged).unwrap();
    assert!(Store::open(root.path(), JOB)
        .err()
        .unwrap()
        .to_string()
        .contains("inconsistent replay index"));
    store.set_return_limit(0).unwrap();
    assert!(!store.append(&chunk(2, 0, b"refused".to_vec())).unwrap());
    assert!(!store
        .append(&chunk(3, 0, b"also refused".to_vec()))
        .unwrap());
    drop(store);
    let store = Store::open(root.path(), JOB).unwrap();
    assert_eq!(store.last_seq(), Some(3));
    assert_eq!(store.index.refused_tail.as_ref().unwrap().first_seq, 2);
    assert_eq!(store.index.refused_tail.as_ref().unwrap().last_seq, 3);
    assert_eq!(
        store
            .frame(0)
            .unwrap()
            .map(|f| decode(&f).unwrap()["seq"].clone()),
        Some(json!(1))
    );
}

#[test]
fn recorded_over_cap_path_explains_an_interior_return_file_gap_after_restart() {
    let root = scratch();
    let mut store = Store::open(root.path(), JOB).unwrap();
    store.set_return_limit(7).unwrap();
    assert!(store.append(&chunk(1, 0, b"abc".to_vec())).unwrap());
    assert!(store.append(&chunk(2, 1, b"de".to_vec())).unwrap());
    assert!(store.append(&chunk(3, 2, b"fg".to_vec())).unwrap());
    assert!(!store.append(&chunk(4, 1, b"more".to_vec())).unwrap());
    assert_eq!(store.index.dropped.len(), 1);
    assert_eq!(store.index.dropped[0].return_path, Some(1));
    drop(store);
    let mut store = Store::open(root.path(), JOB).unwrap();
    assert_eq!(store.last_seq(), Some(4));
    assert_eq!(decode(&store.frame(1).unwrap().unwrap()).unwrap()["seq"], 3);
    store.append(&manifest(5, &[3, 6, 2])).unwrap();
    store.append(&frame(json!({"type":"terminal"}))).unwrap();
    drop(store);
    let store = Store::open(root.path(), JOB).unwrap();
    let replay = drain(&store, 0, 0);
    assert_eq!(
        replay
            .iter()
            .filter(|f| f["type"] == "return_chunk")
            .count(),
        2
    );
    assert_eq!(replay.iter().filter(|f| f["type"] == "terminal").count(), 1);
}
