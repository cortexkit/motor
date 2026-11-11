use super::*;

#[tokio::test]
async fn verified_lease_carries_only_the_saved_host_permission_ceiling() {
    for admitted in [None, Some(false), Some(true)] {
        let (runtime, fake, broker, requester, mut claims) = fixture();
        claims.cache_write = admitted;
        let (mut client, server) = tokio::io::duplex(8192);
        let task = tokio::spawn(serve(runtime.clone(), server));
        let challenge = hello(&mut client, &broker, &requester, &claims).await;
        assert_eq!(
            respond(&mut client, &requester, &claims, challenge).await["type"],
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
        assert_eq!(
            lease.get("cache_write").and_then(|v| v.as_bool()),
            (admitted == Some(true)).then_some(true)
        );
        drop(guest);
        drop(client);
        let _ = task.await.unwrap();
        runtime.host.lock().unwrap().guest_unready("tenant");
    }
}
