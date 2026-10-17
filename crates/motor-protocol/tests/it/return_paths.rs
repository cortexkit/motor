use motor_protocol::*;

#[test]
fn return_paths_are_additive_and_default_fields_are_omitted() {
    let old: Frame = serde_json::from_str(r#"{"type":"hello","protocol_version":1}"#).unwrap();
    assert!(matches!(
        old,
        Frame::Hello {
            return_paths: false,
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
