use super::*;
use motor_host::{
    helper::Control,
    jobs::fill_source::{self, Incoming, Request, Slot},
};
use motor_protocol::{BundleManifest, BytePayload, FillSource, Frame, FILL_SOURCE_MAX_BYTES};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    os::unix::net::UnixStream,
    sync::Mutex,
    time::{Duration, Instant},
};

fn source(bytes: &[u8], tip: char) -> FillSource {
    FillSource {
        repository_root: "/home/source".into(),
        integration_tip: tip.to_string().repeat(40),
        bundles: vec![BundleManifest {
            index: 0,
            repository: "/home/source".into(),
            length: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        }],
        git_dependencies: vec![],
    }
}
fn request(tip: char, observed_ms: u64) -> Request {
    Request {
        account: job(JOB).sub,
        repository_root: "/home/source".into(),
        integration_tip: tip.to_string().repeat(40),
        mem_mib: 100,
        observed_ms,
    }
}
fn chunk(bytes: &[u8], offset: u64) -> Frame {
    Frame::BundleChunk {
        index: 0,
        offset,
        data: BytePayload(bytes.to_vec()),
    }
}

#[test]
fn source_chunks_enforce_offsets_digest_and_cap_and_failed_replacement_keeps_old_bytes() {
    let scratch = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
    let journal = scratch.path();
    let mut incoming = Incoming::new(
        fill_source::directory(journal, "incoming-old"),
        source(b"old", 'a'),
    )
    .unwrap();
    incoming.chunk(chunk(b"old", 0)).unwrap();
    incoming.publish(journal, "old").unwrap();
    let mut oversized = source(b"new", 'b');
    oversized.bundles[0].length = FILL_SOURCE_MAX_BYTES + 1;
    assert!(Incoming::new(fill_source::directory(journal, "oversized"), oversized).is_err());
    for (name, frame) in [
        ("gap", chunk(b"new", 1)),
        ("corrupt", chunk(b"bad", 0)),
        ("overrun", chunk(b"new!", 0)),
        ("empty", chunk(b"", 0)),
    ] {
        let path = fill_source::directory(journal, name);
        let mut incoming = Incoming::new(path.clone(), source(b"new", 'b')).unwrap();
        assert!(incoming.chunk(frame).is_err());
        drop(incoming);
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(fill_source::directory(journal, "old").join("0")).unwrap(),
            b"old"
        );
    }
}

struct SourceHelper {
    reply: Mutex<Vec<u8>>,
}
impl Control for SourceHelper {
    fn job_relay(&self, _: &str, port: u32) -> anyhow::Result<UnixStream> {
        assert_eq!(port, 54);
        let (host, mut guest) = UnixStream::pair()?;
        let bytes = self.reply.lock().unwrap().clone();
        std::thread::spawn(move || {
            assert_eq!(
                motor_host::helper::read_json(&mut guest).unwrap()["op"],
                "export_tip"
            );
            guest.write_all(&bytes).unwrap();
        });
        Ok(host)
    }
    fn adopt(&self, _: &str) -> anyhow::Result<bool> {
        unreachable!()
    }
    fn wake(&self, _: &str, _: Option<u64>) -> anyhow::Result<u64> {
        unreachable!()
    }
    fn quota(&self, _: &str, _: u32) -> anyhow::Result<()> {
        unreachable!()
    }
    fn relay(&self, _: &str, _: u32) -> anyhow::Result<UnixStream> {
        unreachable!()
    }
    fn admin(&self, _: &str, _: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        unreachable!()
    }
    fn suspend(&self, _: &str) -> anyhow::Result<()> {
        unreachable!()
    }
    fn available_bytes(&self) -> anyhow::Result<u64> {
        Ok(u64::MAX)
    }
    fn host_available_mib(&self) -> anyhow::Result<u64> {
        Ok(10000)
    }
    fn online_cpus(&self) -> anyhow::Result<u32> {
        Ok(4)
    }
}
fn response(source: &FillSource, bytes: &[u8]) -> Vec<u8> {
    let mut result = vec![];
    motor_host::helper::write_json(&mut result, &serde_json::json!({"ok":true,"source":source}))
        .unwrap();
    motor_protocol::write_frame(&mut result, &chunk(bytes, 0)).unwrap();
    result
}

#[test]
fn an_export_cancelled_after_all_bytes_arrive_leaves_no_unreserved_generation() {
    let scratch = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
    let host = std::sync::Arc::new(Mutex::new(durable_host(scratch.path(), 10000)));
    let mut original = Incoming::new(
        fill_source::directory(scratch.path(), "incoming-old"),
        source(b"old", 'a'),
    )
    .unwrap();
    original.chunk(chunk(b"old", 0)).unwrap();
    original.publish(scratch.path(), "old").unwrap();
    let old = Slot {
        request: request('a', 1),
        generation: "old".into(),
        source: source(b"old", 'a'),
        bytes: 3,
        last_fill_started_ms: None,
    };
    let key = fill_source::key(&old.request.account, &old.request.repository_root);
    host.lock()
        .unwrap()
        .snapshot
        .per_job
        .fill_sources
        .insert(key.clone(), old.clone());
    host.lock()
        .unwrap()
        .snapshot
        .per_job
        .export_tips
        .insert(JOB.into(), request('b', 2));
    let helper = SourceHelper {
        reply: Mutex::new(response(&source(b"new", 'b'), b"new")),
    };
    let mut finalized = false;
    let error = fill_source::export_before_commit(
        &host,
        &helper,
        JOB,
        Instant::now() + Duration::from_secs(10),
        || {
            finalized = true;
            host.lock()
                .unwrap()
                .snapshot
                .per_job
                .export_tips
                .remove(JOB);
        },
    )
    .unwrap_err();
    assert!(
        finalized,
        "the complete source must reach the finalization boundary"
    );
    assert!(error.to_string().contains("expired"), "{error}");
    assert_eq!(
        host.lock().unwrap().snapshot.per_job.fill_sources[&key],
        old
    );
    let entries: Vec<_> = std::fs::read_dir(scratch.path().join("fill-sources"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, [std::ffi::OsString::from("old")]);
    assert_eq!(
        std::fs::read(fill_source::directory(scratch.path(), "old").join("0")).unwrap(),
        b"old"
    );
}

#[test]
fn a_failed_journal_swap_removes_the_renamed_source_generation() {
    struct FailingJournal(std::path::PathBuf);
    impl motor_host::journal::Journal for FailingJournal {
        fn directory(&self) -> Option<std::path::PathBuf> {
            Some(self.0.clone())
        }
        fn store(&mut self, _: &motor_host::state::Snapshot) -> std::io::Result<()> {
            Err(std::io::Error::other("journal unavailable"))
        }
    }
    let scratch = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
    let host = std::sync::Arc::new(Mutex::new(configure(
        Host::new(
            BTreeMap::new(),
            Default::default(),
            Box::new(FailingJournal(scratch.path().into())),
        ),
        10000,
    )));
    host.lock()
        .unwrap()
        .snapshot
        .per_job
        .export_tips
        .insert(JOB.into(), request('b', 2));
    let helper = SourceHelper {
        reply: Mutex::new(response(&source(b"new", 'b'), b"new")),
    };
    let error = fill_source::export(
        &host,
        &helper,
        JOB,
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap_err();
    assert!(error.to_string().contains("journal unavailable"), "{error}");
    assert_eq!(
        std::fs::read_dir(scratch.path().join("fill-sources"))
            .unwrap()
            .count(),
        0
    );
    assert!(host
        .lock()
        .unwrap()
        .snapshot
        .per_job
        .fill_sources
        .is_empty());
}

#[test]
fn newer_tip_replaces_a_slot_atomically_and_a_corrupt_successor_keeps_it_across_restart() {
    let scratch = tempfile::tempdir_in(cortexkit_test_support::scratch_root()).unwrap();
    let host = std::sync::Arc::new(Mutex::new(durable_host(scratch.path(), 10000)));
    let helper = SourceHelper {
        reply: Mutex::new(vec![]),
    };
    for (tip, bytes, expected) in [
        ('a', b"old".as_slice(), true),
        ('b', b"new".as_slice(), true),
        ('c', b"bad".as_slice(), false),
    ] {
        let source = source(if expected { bytes } else { b"expected" }, tip);
        *helper.reply.lock().unwrap() = response(&source, bytes);
        host.lock()
            .unwrap()
            .snapshot
            .per_job
            .export_tips
            .insert(JOB.into(), request(tip, tip as u64));
        let result =
            fill_source::export(&host, &helper, JOB, Instant::now() + Duration::from_secs(5));
        assert_eq!(result.is_ok(), expected, "{result:?}");
        let host = host.lock().unwrap();
        let key = fill_source::key(&request(tip, 0).account, "/home/source");
        let slot = &host.snapshot.per_job.fill_sources[&key];
        assert_eq!(
            slot.request.integration_tip,
            if tip == 'a' {
                "a".repeat(40)
            } else {
                "b".repeat(40)
            }
        );
        assert_eq!(
            std::fs::read(
                fill_source::directory(&host.journal_directory().unwrap(), &slot.generation)
                    .join("0")
            )
            .unwrap(),
            if tip == 'a' { b"old" } else { b"new" }
        );
        assert!(host.snapshot.per_job.export_tips.is_empty());
    }
    let kept = host.lock().unwrap().snapshot.per_job.fill_sources.clone();
    host.lock()
        .unwrap()
        .snapshot
        .per_job
        .export_tips
        .insert(JOB.into(), request('d', 200));
    let expired = fill_source::export(&host, &helper, JOB, Instant::now() - Duration::from_secs(1))
        .unwrap_err();
    assert!(expired.to_string().contains("timed out"));
    assert_eq!(host.lock().unwrap().snapshot.per_job.fill_sources, kept);
    assert!(host.lock().unwrap().snapshot.per_job.export_tips.is_empty());
    assert_eq!(motor_protocol::FILL_SOURCE_EXPORT_TIMEOUT_S, 300);
    drop(host);
    let reopened = durable_host(scratch.path(), 10000);
    assert_eq!(reopened.snapshot.per_job.fill_sources.len(), 1);
    assert_eq!(
        reopened
            .snapshot
            .per_job
            .fill_sources
            .values()
            .next()
            .unwrap()
            .source
            .integration_tip,
        "b".repeat(40)
    );
}

#[test]
fn source_identity_is_per_account_and_repository_and_default_cap_is_one_gib() {
    assert_ne!(
        fill_source::key("account-a", "/home/repo"),
        fill_source::key("account-b", "/home/repo")
    );
    assert_ne!(
        fill_source::key("account-a", "/home/repo"),
        fill_source::key("account-a", "/home/other")
    );
    assert_eq!(FILL_SOURCE_MAX_BYTES, 1 << 30);
    let slot = Slot {
        request: request('a', AT),
        generation: "kept".into(),
        source: source(b"old", 'a'),
        bytes: 3,
        last_fill_started_ms: None,
    };
    assert_eq!(
        slot.bytes,
        slot.source.validate(FILL_SOURCE_MAX_BYTES).unwrap()
    );
}
