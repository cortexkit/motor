use motor_protocol::*;

#[test]
fn return_paths_are_additive_and_default_fields_are_omitted() {
    let old: Frame = serde_json::from_str(r#"{"type":"hello","protocol_version":1}"#).unwrap();
    assert!(matches!(
        old,
        Frame::Hello {
            return_paths: false,
            return_chunked: false,
            ..
        }
    ));
    assert!(!serde_json::to_value(old)
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("return_paths"));
    let mut value: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../test-vectors/exec-remote-v1/frames/run.json"
    ))
    .unwrap();
    value.as_object_mut().unwrap().remove("return_paths");
    value.as_object_mut().unwrap().remove("return_max_bytes");
    let run: Frame = serde_json::from_value(value).unwrap();
    assert!(
        matches!(&run, Frame::Run(r) if r.return_paths.is_empty() && r.return_max_bytes == RETURN_MAX_BYTES)
    );
    let value = serde_json::to_value(run).unwrap();
    assert!(value.get("return_paths").is_none());
    assert!(value.get("return_max_bytes").is_none());
}

#[test]
fn return_path_request_bounds_are_lexical_and_byte_counted() {
    let valid = vec!["/workspace/out".into()];
    assert!(validate_return_paths("/workspace", &valid, 1).is_ok());
    for (paths, cap) in [
        (valid.clone(), RETURN_MAX_BYTES + 1),
        (vec!["/workspace/out".into(); 65], 1),
        (vec![format!("/workspace/{}", "é".repeat(2043))], 1),
        (vec!["/workspace-other/out".into()], 1),
        (vec!["/workspace/../secret".into()], 1),
        (vec!["out".into()], 1),
    ] {
        assert_eq!(
            validate_return_paths("/workspace", &paths, cap),
            Err("return_paths_exceeded")
        );
    }
}

#[test]
fn return_chunked_fields_are_additive_and_omitted_when_false() {
    let run_vector: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../test-vectors/exec-remote-v1/frames/run.json"
    ))
    .unwrap();
    let Frame::Run(mut run) = serde_json::from_value(run_vector.clone()).unwrap() else {
        panic!("run vector")
    };
    assert!(!run.return_chunked);
    // Without the opt-in, the Run frame keeps its exact bytes.
    assert_eq!(
        serde_json::to_value(Frame::Run(run.clone())).unwrap(),
        run_vector
    );
    run.return_chunked = true;
    let wire = serde_json::to_value(Frame::Run(run)).unwrap();
    assert_eq!(wire["return_chunked"], true);
    let Frame::Run(decoded) = serde_json::from_value(wire).unwrap() else {
        panic!("run")
    };
    assert!(decoded.return_chunked);

    let hello: Frame =
        serde_json::from_str(r#"{"type":"hello","protocol_version":1,"return_paths":true}"#)
            .unwrap();
    let Frame::Hello { return_chunked, .. } = hello.clone() else {
        panic!("hello")
    };
    assert!(
        !return_chunked,
        "an older runner's Hello advertises no chunking"
    );
    assert!(serde_json::to_value(hello)
        .unwrap()
        .get("return_chunked")
        .is_none());
    let hello: Frame =
        serde_json::from_str(r#"{"type":"hello","protocol_version":1,"return_chunked":true}"#)
            .unwrap();
    assert!(matches!(
        hello,
        Frame::Hello {
            return_chunked: true,
            ..
        }
    ));
}

#[test]
fn return_chunked_raises_the_cap_only_with_the_opt_in() {
    let paths = vec!["/workspace/out".to_string()];
    assert_eq!(
        validate_return_request("/workspace", &paths, RETURN_MAX_BYTES + 1, false),
        Err("return_paths_exceeded")
    );
    assert!(validate_return_request("/workspace", &paths, RETURN_MAX_BYTES + 1, true).is_ok());
    assert!(
        validate_return_request("/workspace", &paths, RETURN_CHUNKED_MAX_BYTES_CAP, true).is_ok()
    );
    assert_eq!(
        validate_return_request("/workspace", &paths, RETURN_CHUNKED_MAX_BYTES_CAP + 1, true),
        Err("return_paths_exceeded")
    );
    let many = vec!["/workspace/out".to_string(); 65];
    assert_eq!(
        validate_return_request("/workspace", &many, 1, true),
        Err("return_paths_exceeded")
    );
    let outside = vec!["/elsewhere/out".to_string()];
    assert_eq!(
        validate_return_request("/workspace", &outside, 1, true),
        Err("return_paths_exceeded")
    );
}

#[test]
fn a_full_return_chunk_frame_fits_the_frame_cap() {
    let chunk = ReturnChunk::new(
        Uuid::nil(),
        u64::MAX,
        63,
        u64::MAX,
        BytePayload(vec![0xff; RETURN_CHUNK_MAX_BYTES]),
    );
    let frame = Frame::ReturnChunk(chunk);
    let bytes = serde_json::to_vec(&frame).unwrap();
    assert!(bytes.len() <= MAX_FRAME_BYTES, "{} bytes", bytes.len());
    let mut encoded = Vec::new();
    write_frame(&mut encoded, &frame).unwrap();
    assert_eq!(read_frame(&mut encoded.as_slice()).unwrap(), Some(frame));
}

/// The crate's own `return-chunked` vectors, located through Cargo so the
/// test reads exactly the pinned release.
fn shared_vectors() -> std::path::PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--offline",
            "--manifest-path",
        ])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        // Nested Cargo must not pick up a compile wrapper the sandbox blocks.
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let manifest = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == "cortexkit-exec-remote-types")
        .expect("cortexkit-exec-remote-types is a dependency")["manifest_path"]
        .as_str()
        .unwrap()
        .to_owned();
    std::path::Path::new(&manifest)
        .parent()
        .unwrap()
        .join("test-vectors/exec-remote-v1/return-chunked")
}

/// What the shared checker makes of a vector's records, in the vector's
/// `expect` shape.
fn assemble(case: &serde_json::Value) -> serde_json::Value {
    use serde_json::json;
    let paths: Vec<String> = serde_json::from_value(case["return_paths"].clone()).unwrap();
    let mut assembler = ReturnAssembler::new(&paths, case["cap_bytes"].as_u64().unwrap());
    let mut written = vec![Vec::new(); paths.len()];
    let error = |error: ReturnStreamError| {
        let mut value = json!({"code": error.code(), "path_index": error.path_index()});
        if let Some(offset) = error.offset() {
            value["offset"] = json!(offset);
        }
        json!({ "error": value })
    };
    for raw in case["records"].as_array().unwrap() {
        match serde_json::from_value::<StreamRecord>(raw.clone()).unwrap() {
            StreamRecord::ReturnChunk(chunk) => match assembler.accept_chunk(&chunk) {
                Ok(data) => written[chunk.path_index as usize].extend_from_slice(data),
                Err(e) => return error(e),
            },
            StreamRecord::ReturnManifest(manifest) => {
                let verified = match assembler.finish(&manifest) {
                    Ok(verified) => verified,
                    Err(e) => return error(e),
                };
                let files: Vec<_> = verified
                    .iter()
                    .map(|file| {
                        let mut value =
                            json!({"path_index": file.path_index(), "path": file.path()});
                        match file.outcome() {
                            ReturnManifestOutcome::Returned { size, sha256 } => {
                                value["size"] = json!(size);
                                value["sha256"] = json!(sha256);
                                value["data"] = serde_json::to_value(BytePayload(
                                    written[file.path_index() as usize].clone(),
                                ))
                                .unwrap();
                            }
                            ReturnManifestOutcome::Failed { cause, size } => {
                                value["cause"] = json!(cause);
                                if let Some(size) = size {
                                    value["size"] = json!(size);
                                }
                            }
                            other => panic!("unexpected outcome {other:?}"),
                        }
                        value
                    })
                    .collect();
                return json!({ "files": files });
            }
            _ => {}
        }
    }
    panic!("no return_manifest")
}

#[test]
fn shared_return_chunked_vectors_pass_as_stream_records_and_runner_frames() {
    use sha2::{Digest, Sha256};
    let root = shared_vectors();
    let mut cases = 0;
    for entry in std::fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "jcs") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(path.with_extension("sha256")).unwrap(),
            format!("{:x}", Sha256::digest(&bytes)),
            "{name} SHA-256"
        );
        let case: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for raw in case["records"].as_array().unwrap() {
            let canonical = serde_json_canonicalizer::to_vec(raw).unwrap();
            let record: StreamRecord = serde_json::from_value(raw.clone()).unwrap();
            assert_eq!(
                serde_json_canonicalizer::to_vec(&record).unwrap(),
                canonical,
                "{name}: record round-trips"
            );
            // The runner sends the same records as frames, byte for byte.
            if raw["type"] == "return_chunk" || raw["type"] == "return_manifest" {
                let frame: Frame = serde_json::from_value(raw.clone()).unwrap();
                assert!(matches!(
                    frame,
                    Frame::ReturnChunk(_) | Frame::ReturnManifest(_)
                ));
                assert_eq!(
                    serde_json_canonicalizer::to_vec(&frame).unwrap(),
                    canonical,
                    "{name}: frame round-trips"
                );
            }
        }
        assert_eq!(assemble(&case), case["expect"], "{name}");
        cases += 1;
    }
    assert_eq!(
        cases, 21,
        "the 0.2.10 release has 21 return-chunked vectors"
    );
}
