use super::*;
use crate::motor::submission_key::KeyRecord;
use cortexkit_test_support::ScratchDir;

#[test]
fn submission_key_retention_matches_job_and_expired_key_is_unknown() {
    let scratch = ScratchDir::new("key-retention");
    let root = scratch.join("state");
    let storage = Storage::open(&root).unwrap();
    let id = Uuid::now_v7();
    let key = KeyRecord {
        principal: "reserved:aft".into(),
        key: "caller:submission-0001".into(),
        digest: "digest".into(),
        run_sent: false,
        accepted: None,
    };
    storage.create(id, 30, None, None, false).unwrap();
    storage.record_submission(id, key.clone()).unwrap();
    storage.terminal_observed(id).unwrap();
    storage.terminal_delivered(id).unwrap();
    let retained = storage.job(id).unwrap();
    assert!(retained.retain_until_ms.unwrap() >= now_ms() + RETENTION_MS - 1000);
    assert_eq!(
        storage
            .keyed_job(&key.principal, &key.key)
            .unwrap()
            .unwrap()
            .0,
        id
    );
    drop(storage);
    let storage = Storage::open(&root).unwrap();
    assert_eq!(storage.job(id).unwrap().submission, Some(key.clone()));
    storage
        .update(|state| state.jobs.get_mut(&id).unwrap().retain_until_ms = Some(0))
        .unwrap();
    assert!(storage.job(id).is_none());
    assert!(storage
        .keyed_job(&key.principal, &key.key)
        .unwrap()
        .is_none());
}
