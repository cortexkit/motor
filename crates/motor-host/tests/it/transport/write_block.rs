use super::*;

#[test]
fn admin_block_persists_and_names_all_unacknowledged_runners() {
    let (runtime, fake, _, _, _) = fixture();
    fake.running.store(true, Ordering::Release);
    let mut other = runtime.host.lock().unwrap().tenants["vm"].clone();
    other.tenant = "other".into();
    runtime
        .host
        .lock()
        .unwrap()
        .tenants
        .insert("other-vm".into(), other);
    *fake.admin_reply.lock().unwrap() = Some(json!({"ok":false,"error":"unsupported"}));
    let request = json!({"op":"set_remote_cache_write_block","blocked":true});
    assert_eq!(
        runtime.host_admin(&request).unwrap(),
        json!({"ok":true,"blocked":true,"not_acknowledged":["other","tenant"]})
    );
    assert!(
        runtime
            .host
            .lock()
            .unwrap()
            .snapshot
            .remote_cache_write_blocked
    );
    assert_eq!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| *c == "admin:set_remote_cache_write_block")
            .count(),
        2
    );
    assert!(runtime.admin("tenant", &request).is_err());
    for invalid in [
        json!({"op":"set_remote_cache_write_block"}),
        json!({"op":"set_remote_cache_write_block","blocked":1}),
        json!({"op":"set_remote_cache_write_block","blocked":false,"grant":true}),
    ] {
        assert!(runtime.host_admin(&invalid).is_err());
        assert!(
            runtime
                .host
                .lock()
                .unwrap()
                .snapshot
                .remote_cache_write_blocked
        );
    }
    *fake.admin_reply.lock().unwrap() = None;
    let reply = runtime
        .host_admin(&json!({"op":"set_remote_cache_write_block","blocked":false}))
        .unwrap();
    assert_eq!(reply["not_acknowledged"], json!([]));
    assert!(
        !runtime
            .host
            .lock()
            .unwrap()
            .snapshot
            .remote_cache_write_blocked
    );
}

async fn wake_with_block(acknowledges: bool) {
    let (runtime, fake, broker, requester, c) = fixture();
    assert_eq!(
        runtime
            .host_admin(&json!({"op":"set_remote_cache_write_block","blocked":true}))
            .unwrap()["not_acknowledged"],
        json!([])
    );
    assert!(fake.calls.lock().unwrap().is_empty());
    if !acknowledges {
        *fake.admin_reply.lock().unwrap() = Some(json!({"ok":false,"error":"unsupported"}));
    }
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(serve(runtime.clone(), server));
    let challenge = hello(&mut client, &broker, &requester, &c).await;
    assert_eq!(
        respond(&mut client, &requester, &c, challenge).await["type"],
        "lease_ok"
    );
    write_frame(&mut client, &json!({"type":"hello"}))
        .await
        .unwrap();
    let guest = peer(&fake).await;
    guest.set_nonblocking(true).unwrap();
    let mut guest = tokio::net::UnixStream::from_std(guest).unwrap();
    let lease = read_frame(&mut guest).await.unwrap();
    assert_eq!(lease["type"], "verified_lease");
    assert_ne!(lease["cache_write"], true);
    assert!(runtime.host.lock().unwrap().snapshot.leases[JOB].delivered);
    let calls = fake.calls.lock().unwrap().clone();
    let wake = calls.iter().position(|c| c == "wake").unwrap();
    let deny = calls
        .iter()
        .position(|c| c == "admin:set_remote_cache_write_block")
        .unwrap();
    let relay = calls.iter().position(|c| c == "relay:52").unwrap();
    assert!(wake < deny && deny < relay, "{calls:?}");
    drop(guest);
    drop(client);
    let _ = task.await.unwrap();
    runtime.host.lock().unwrap().guest_unready("tenant");
}

#[tokio::test]
async fn runner_waking_after_block_gets_deny_before_read_only_lease_delivery() {
    wake_with_block(true).await;
}

#[tokio::test]
async fn older_runner_refusal_does_not_prevent_read_only_admission() {
    wake_with_block(false).await;
}
