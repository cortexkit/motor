use super::*;
use crate::jobs::fill_source::{self, Request, Slot};
use motor_protocol::{BundleManifest, FillSource};
use sha2::{Digest, Sha256};

fn set_forks(runtime: &Runtime, enabled: bool) {
    let crate::config::HostSettings::PerJob(settings) = &mut runtime.host.lock().unwrap().settings
    else {
        unreachable!()
    };
    settings.fork_checkouts = enabled;
}

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
        set_forks(&runtime, false);
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

fn duplicate_source_slot() -> Slot {
    let request = Request {
        account: claims(JOB).sub,
        repository_root: "/home/source".into(),
        integration_tip: "a".repeat(40),
        mem_mib: 100,
        observed_ms: unix_ms(),
    };
    let mut source = FillSource {
        repository_root: request.repository_root.clone(),
        integration_tip: request.integration_tip.clone(),
        bundles: vec![],
        git_dependencies: vec![],
    };
    for index in 0..3 {
        let bundle = BundleManifest {
            index,
            repository: if index == 0 {
                request.repository_root.clone()
            } else {
                "https://example.invalid/dependency".into()
            },
            length: 1,
            sha256: format!("{:x}", Sha256::digest(b"x")),
        };
        if index != 0 {
            source.git_dependencies.push(motor_protocol::GitDependency {
                url: bundle.repository.clone(),
                commits: vec!["c".repeat(40)],
                bundle: Some(bundle.clone()),
            });
        }
        source.bundles.push(bundle);
    }
    source
        .validate(motor_protocol::FILL_SOURCE_MAX_BYTES)
        .unwrap();
    Slot {
        request,
        generation: "duplicate-source".into(),
        source,
        bytes: 3,
        last_fill_started_ms: Some(unix_ms()),
    }
}

#[test]
fn duplicate_exports_are_refused_before_allocating_host_storage() {
    let (_, _, storage) = fixture();
    let slot = duplicate_source_slot();
    let path = fill_source::directory(storage.root.path(), "incoming-duplicate");
    let result = fill_source::Incoming::new(path.clone(), slot.source);
    let error = result
        .err()
        .expect("duplicate source must be refused")
        .to_string();
    assert!(
        error.contains("https://example.invalid/dependency"),
        "{error}"
    );
    assert!(error.contains(&"c".repeat(40)), "{error}");
    assert!(!path.exists());
}

#[test]
fn candidate_read_retires_stored_duplicate_sources_and_their_bundles() {
    use crate::jobs::fills::FillGuest;

    let (runtime, _, _storage) = fixture();
    set_forks(&runtime, false);
    let slot = duplicate_source_slot();
    let key = fill_source::key(&slot.request.account, &slot.request.repository_root);
    let journal = {
        let mut host = runtime.host.lock().unwrap();
        let journal = host.journal_directory().unwrap();
        let path = fill_source::directory(&journal, &slot.generation);
        std::fs::create_dir_all(&path).unwrap();
        for index in 0..3 {
            std::fs::write(path.join(index.to_string()), b"x").unwrap();
        }
        let mut next = host.snapshot.clone();
        next.per_job.fill_sources.insert(key, slot.clone());
        host.save(next).unwrap();
        journal
    };
    let guest = crate::jobs::fills::BundleFillGuest {
        host: runtime.host.clone(),
        resolvers: Default::default(),
    };
    assert!(guest.candidates().unwrap().is_empty());
    assert!(runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .fill_sources
        .is_empty());
    assert!(!fill_source::directory(&journal, &slot.generation).exists());
    drop(guest);
    drop(runtime);
    let (_, snapshot) = crate::journal::FileJournal::open(&journal).unwrap();
    assert!(snapshot.per_job.fill_sources.is_empty());
}

fn valid_source_slot() -> Slot {
    let mut slot = duplicate_source_slot();
    slot.source.bundles.truncate(1);
    slot.source.git_dependencies.clear();
    slot.bytes = 1;
    slot
}

#[test]
fn checkout_forks_retire_persisted_sources_and_pending_exports_but_other_modes_keep_them() {
    for mode in ["forks", "fills", "tenant"] {
        let (runtime, _, storage) = fixture();
        let slot = valid_source_slot();
        let key = fill_source::key(&slot.request.account, &slot.request.repository_root);
        let directory = fill_source::directory(storage.root.path(), &slot.generation);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("0"), b"x").unwrap();
        claim(&runtime, &claims(JOB));
        {
            let mut host = runtime.host.lock().unwrap();
            let mut next = host.snapshot.clone();
            next.per_job
                .export_tips
                .insert(JOB.into(), slot.request.clone());
            next.per_job.fill_sources.insert(key.clone(), slot.clone());
            host.save(next).unwrap();
            match mode {
                "fills" => {
                    let crate::config::HostSettings::PerJob(settings) = &mut host.settings else {
                        unreachable!()
                    };
                    settings.fork_checkouts = false;
                }
                "tenant" => host.settings = crate::config::HostSettings::Tenant,
                _ => {}
            }
            host.clean_fill_sources().unwrap();
            if mode == "forks" {
                assert!(host.snapshot.per_job.export_tips.is_empty());
                assert!(host.snapshot.per_job.fill_sources.is_empty());
            } else {
                assert_eq!(host.snapshot.per_job.export_tips[JOB], slot.request);
                assert_eq!(host.snapshot.per_job.fill_sources[&key], slot);
            }
        }
        assert_eq!(directory.exists(), mode != "forks");
        drop(runtime);
        let (_, snapshot) = crate::journal::FileJournal::open(storage.root.path()).unwrap();
        assert_eq!(snapshot.per_job.fill_sources.is_empty(), mode == "forks");
        assert_eq!(snapshot.per_job.export_tips.is_empty(), mode == "forks");
    }
}

#[test]
fn disabling_checkout_forks_rebuilds_a_source_from_the_next_default_branch_job() {
    let (runtime, fake, _storage) = fixture();
    set_forks(&runtime, false);
    let mut value = run(JOB);
    value["integration_tip"] = json!("a".repeat(40));
    let motor_protocol::Frame::Run(mut run) = serde_json::from_value(value).unwrap() else {
        unreachable!()
    };
    claim(&runtime, &claims(JOB));
    runtime
        .host
        .lock()
        .unwrap()
        .note_fill_source(JOB, &run, unix_ms())
        .unwrap();
    assert_eq!(
        runtime
            .host
            .lock()
            .unwrap()
            .snapshot
            .per_job
            .export_tips
            .len(),
        1
    );
    set_forks(&runtime, true);
    {
        let mut host = runtime.host.lock().unwrap();
        host.clean_fill_sources().unwrap();
        host.note_fill_source(JOB, &run, unix_ms()).unwrap();
        assert!(host.snapshot.per_job.export_tips.is_empty());
    }
    // Retiring sources when forks turn on must not permanently disable exports.
    set_forks(&runtime, false);
    claim(&runtime, &claims(JOB2));
    run.job_id = JOB2.parse().unwrap();
    let request = {
        let mut host = runtime.host.lock().unwrap();
        host.note_fill_source(JOB2, &run, unix_ms()).unwrap();
        host.snapshot.per_job.export_tips[JOB2].clone()
    };
    let bytes = b"fresh source";
    let source = FillSource {
        repository_root: request.repository_root.clone(),
        integration_tip: request.integration_tip.clone(),
        bundles: vec![BundleManifest {
            index: 0,
            repository: request.repository_root.clone(),
            length: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        }],
        git_dependencies: vec![],
    };
    let (socket, mut guest) = UnixStream::pair().unwrap();
    *fake.fill_export.lock().unwrap() = Some(socket);
    let guest = std::thread::spawn(move || {
        assert_eq!(
            crate::helper::read_json(&mut guest).unwrap()["op"],
            "export_tip"
        );
        crate::helper::write_json(&mut guest, &json!({"ok":true,"source":source})).unwrap();
        motor_protocol::write_frame(
            &mut guest,
            &motor_protocol::Frame::BundleChunk {
                index: 0,
                offset: 0,
                data: motor_protocol::BytePayload(bytes.to_vec()),
            },
        )
        .unwrap();
    });
    fill_source::export(
        &runtime.host,
        &*fake,
        JOB2,
        std::time::Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    guest.join().unwrap();
    let key = fill_source::key(&request.account, &request.repository_root);
    {
        let host = runtime.host.lock().unwrap();
        assert!(host.snapshot.per_job.export_tips.is_empty());
        let slot = &host.snapshot.per_job.fill_sources[&key];
        assert_eq!(slot.request, request);
        assert_eq!(
            std::fs::read(
                fill_source::directory(&host.journal_directory().unwrap(), &slot.generation)
                    .join("0")
            )
            .unwrap(),
            bytes
        );
    }
    set_forks(&runtime, true);
    runtime.host.lock().unwrap().clean_fill_sources().unwrap();
    assert!(runtime
        .host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .fill_sources
        .is_empty());
}
