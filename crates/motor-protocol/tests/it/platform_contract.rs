use motor_protocol::*;
use serde_json::json;

const LINUX_RUN: &[u8] = br#"{"type":"run","job_id":"00000000-0000-0000-0000-000000000000","workspace_key":"/work","repository_root":"/source","cwd":"/work","command":"true","env":{},"queue_wait_limit_s":300,"snapshot_commit":"commit","tree_hash":"tree","integration_tip":"tip","siblings":[],"bundles":[]}"#;
const LEGACY_STARTED: &[u8] = br#"{"type":"started","job_id":"00000000-0000-0000-0000-000000000000","seq":0,"queue_wait_ms":25,"started_at_ms":125}"#;

fn run() -> Run {
    let Frame::Run(run) = serde_json::from_slice(LINUX_RUN).unwrap() else {
        panic!("expected Run");
    };
    run
}

#[test]
fn linux_run_without_platform_keeps_legacy_wire_bytes() {
    let run = run();
    assert_eq!(run.platform, None);
    let frame = Frame::Run(run);
    assert_eq!(serde_json::to_vec(&frame).unwrap(), LINUX_RUN);
    let mut wire = Vec::new();
    write_frame(&mut wire, &frame).unwrap();
    assert_eq!(&wire[..4], &(LINUX_RUN.len() as u32).to_be_bytes());
    assert_eq!(&wire[4..], LINUX_RUN);
    assert_eq!(read_frame(&mut &wire[..]).unwrap(), Some(frame));
}

#[test]
fn run_platform_uses_shared_tags_and_preserves_unknown_values() {
    for (platform, tag) in [
        (Platform::Linux, "linux"),
        (Platform::Windows, "windows"),
        (Platform::Unknown("future-os".into()), "future-os"),
    ] {
        let mut run = run();
        run.platform = Some(platform.clone());
        let frame = Frame::Run(run);
        let encoded = serde_json::to_value(&frame).unwrap();
        assert_eq!(encoded["platform"], tag);
        let decoded: Frame = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, frame);
        let Frame::Run(decoded) = decoded else {
            panic!("expected Run");
        };
        assert_eq!(decoded.platform, Some(platform));
    }
}

#[test]
fn hello_empty_platforms_keep_both_legacy_wire_shapes() {
    for (legacy, started) in [
        (
            br#"{"type":"hello","protocol_version":1}"#.as_slice(),
            false,
        ),
        (
            br#"{"type":"hello","protocol_version":1,"started":true}"#.as_slice(),
            true,
        ),
    ] {
        let frame: Frame = serde_json::from_slice(legacy).unwrap();
        assert_eq!(
            frame,
            Frame::Hello {
                protocol_version: 1,
                return_paths: false,
                started,
                default_branch_tip: false,
                gate_jobs: false,
                platforms: Vec::new(),
            }
        );
        assert_eq!(serde_json::to_vec(&frame).unwrap(), legacy);
    }
    let explicit_empty: Frame = serde_json::from_value(json!({
        "type": "hello", "protocol_version": 1, "platforms": []
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_vec(&explicit_empty).unwrap(),
        br#"{"type":"hello","protocol_version":1}"#
    );
}

#[test]
fn windows_hello_advertises_explicit_platforms() {
    let hello = Frame::Hello {
        return_paths: false,
        protocol_version: 1,
        started: true,
        default_branch_tip: false,
        gate_jobs: false,
        platforms: vec!["windows".into()],
    };
    let expected =
        br#"{"type":"hello","protocol_version":1,"started":true,"platforms":["windows"]}"#;
    assert_eq!(serde_json::to_vec(&hello).unwrap(), expected);
    assert_eq!(serde_json::from_slice::<Frame>(expected).unwrap(), hello);
}

#[test]
fn accepted_platform_echo_is_optional_and_round_trips() {
    let legacy = br#"{"type":"accepted","job_id":"00000000-0000-0000-0000-000000000000","queue_position":0}"#;
    let frame: Frame = serde_json::from_slice(legacy).unwrap();
    let Frame::Accepted(accepted) = frame else {
        panic!("expected Accepted");
    };
    assert_eq!(accepted.platform, None);
    assert_eq!(
        serde_json::to_vec(&Frame::Accepted(accepted.clone())).unwrap(),
        legacy
    );
    for (platform, tag) in [(Platform::Linux, "linux"), (Platform::Windows, "windows")] {
        let accepted = accepted.clone().with_platform(platform.clone());
        assert_eq!(accepted.platform, Some(platform));
        let frame = Frame::Accepted(accepted);
        let encoded = serde_json::to_value(&frame).unwrap();
        assert_eq!(encoded["platform"], tag);
        assert_eq!(serde_json::from_value::<Frame>(encoded).unwrap(), frame);
    }
}

#[test]
fn platform_unsupported_uses_published_refusal_reason() {
    let reason = RefusalReason::PlatformUnsupported;
    assert_eq!(
        serde_json::to_string(&reason).unwrap(),
        "\"platform_unsupported\""
    );
    assert_eq!(
        serde_json::from_str::<RefusalReason>("\"platform_unsupported\"").unwrap(),
        reason
    );
    let request =
        RunRequest::new("/work", "/source", "/work", "true").with_platform(Platform::Windows);
    assert_eq!(
        serde_json::to_value(&request).unwrap()["platform"],
        "windows"
    );
}

#[test]
fn started_mapping_is_optional_and_absence_keeps_legacy_wire_bytes() {
    let frame: Frame = serde_json::from_slice(LEGACY_STARTED).unwrap();
    assert_eq!(
        frame,
        Frame::Started {
            job_id: Uuid::nil(),
            seq: 0,
            queue_wait_ms: 25,
            started_at_ms: 125,
            path_mapping: None,
        }
    );
    assert_eq!(serde_json::to_vec(&frame).unwrap(), LEGACY_STARTED);
}

#[test]
fn started_path_mapping_round_trips_native_windows_prefixes() {
    let frame = Frame::Started {
        job_id: Uuid::nil(),
        seq: 0,
        queue_wait_ms: 25,
        started_at_ms: 125,
        path_mapping: Some(PathMapping::new("/", r"C:\m")),
    };
    let expected = br#"{"type":"started","job_id":"00000000-0000-0000-0000-000000000000","seq":0,"queue_wait_ms":25,"started_at_ms":125,"path_mapping":{"host_prefix":"/","guest_prefix":"C:\\m"}}"#;
    assert_eq!(serde_json::to_vec(&frame).unwrap(), expected);
    assert_eq!(serde_json::from_slice::<Frame>(expected).unwrap(), frame);
    let mut wire = Vec::new();
    write_frame(&mut wire, &frame).unwrap();
    assert_eq!(read_frame(&mut &wire[..]).unwrap(), Some(frame));
}

#[test]
fn shared_started_decoder_preserves_published_path_mapping() {
    let frame = json!({
        "type": "started",
        "job_id": Uuid::nil(),
        "seq": 0,
        "queue_wait_ms": 25,
        "started_at_ms": 125,
        "path_mapping": { "host_prefix": "/", "guest_prefix": r"C:\m" }
    });
    let started: Started = serde_json::from_value(frame).unwrap();
    assert_eq!(
        started,
        Started::new(0, 25, 125).with_path_mapping(PathMapping::new("/", r"C:\m"))
    );
    assert_eq!(
        serde_json::to_value(started).unwrap(),
        json!({ "seq": 0, "queue_wait_ms": 25, "started_at_ms": 125,
            "path_mapping": { "host_prefix": "/", "guest_prefix": r"C:\m" } })
    );
}
