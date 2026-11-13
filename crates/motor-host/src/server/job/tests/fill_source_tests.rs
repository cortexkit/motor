use super::*;
use crate::jobs::fill_source::{self, Request, Slot};
use motor_protocol::{BundleManifest, FillSource};
use sha2::{Digest, Sha256};

fn incomplete_reply() -> Value {
    json!({"ok":true,"skipped":{
        "reason":"fill_source_incomplete",
        "url":"https://example.invalid/dependency",
        "commit":"c".repeat(40),
    }})
}

#[tokio::test]
async fn incomplete_export_keeps_previous_source_and_clears_reservation_only_once() {
    let valid = incomplete_reply();
    let mut unknown = valid.clone();
    unknown["skipped"]["reason"] = json!("unknown");
    let mut missing_commit = valid.clone();
    missing_commit["skipped"]
        .as_object_mut()
        .unwrap()
        .remove("commit");
    let mut ambiguous = valid.clone();
    ambiguous["source"] = Value::Null;
    for (reply, skipped) in [
        (valid, true),
        (unknown, false),
        (missing_commit, false),
        (ambiguous, false),
        (json!({"ok":false,"error":"export failure"}), false),
    ] {
        let (runtime, fake, storage) = fixture();
        let request = Request {
            account: claims(JOB).sub,
            repository_root: "/home/source".into(),
            integration_tip: "a".repeat(40),
            mem_mib: 100,
            observed_ms: unix_ms(),
        };
        let bytes = b"previous source";
        let source = FillSource {
            repository_root: request.repository_root.clone(),
            integration_tip: "b".repeat(40),
            bundles: vec![BundleManifest {
                index: 0,
                repository: request.repository_root.clone(),
                length: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(bytes)),
            }],
            git_dependencies: vec![],
        };
        source
            .validate(motor_protocol::FILL_SOURCE_MAX_BYTES)
            .unwrap();
        let previous = Slot {
            request: Request {
                integration_tip: source.integration_tip.clone(),
                ..request.clone()
            },
            generation: "previous".into(),
            source,
            bytes: bytes.len() as u64,
            last_fill_started_ms: Some(123),
        };
        let key = fill_source::key(&request.account, &request.repository_root);
        {
            let mut host = runtime.host.lock().unwrap();
            let mut next = host.snapshot.clone();
            next.per_job.export_tips.insert(JOB.into(), request);
            next.per_job
                .fill_sources
                .insert(key.clone(), previous.clone());
            host.save(next).unwrap();
        }
        let directory = fill_source::directory(storage.root.path(), "previous");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("0"), bytes).unwrap();
        let (export_host, mut export_guest) = UnixStream::pair().unwrap();
        *fake.fill_export.lock().unwrap() = Some(export_host);
        let guest = std::thread::spawn(move || {
            let request = crate::helper::read_json(&mut export_guest).unwrap();
            assert_eq!(request["op"], "export_tip");
            crate::helper::write_json(&mut export_guest, &reply).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let result = fill_source::export(&runtime.host, &*fake, JOB, deadline);
        guest.join().unwrap();
        assert_eq!(result.is_ok(), skipped, "{result:?}");
        {
            let host = runtime.host.lock().unwrap();
            assert!(host.snapshot.per_job.export_tips.is_empty());
            assert_eq!(host.snapshot.per_job.fill_sources.len(), 1);
            assert_eq!(host.snapshot.per_job.fill_sources[&key], previous);
        }
        assert_eq!(std::fs::read(directory.join("0")).unwrap(), bytes);
        assert_eq!(
            std::fs::read_dir(directory.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
        // A second attempt has no reservation and must not ask the guest again.
        fill_source::export(&runtime.host, &*fake, JOB, deadline).unwrap();
    }
}

#[test]
fn older_host_source_decode_refuses_the_additive_skip_reply() {
    let reply = incomplete_reply();
    assert_eq!(reply["ok"], true);
    // Older hosts decode source immediately after checking ok, before writing
    // any bundle files or replacing the stored source.
    let error = serde_json::from_value::<FillSource>(reply["source"].clone()).unwrap_err();
    assert!(error.to_string().contains("invalid type: null"), "{error}");
}
