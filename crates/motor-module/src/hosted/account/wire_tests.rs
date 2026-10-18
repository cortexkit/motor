use super::*;
use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::{authenticate_server, read_frame, write_frame};

async fn response(stream: &mut tokio::net::TcpStream, request: &Frame, body: Value) {
    let frame = Frame::build(
        FrameType::Response,
        Flags::new(false, Priority::Interactive, false),
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
}

#[tokio::test]
async fn account_vault_transport_uses_authenticated_claustrum_route_and_exact_envelope() {
    let root = cortexkit_test_support::scratch_root()
        .join(format!("account-wire-{}", motor_protocol::Uuid::now_v7()));
    std::fs::create_dir_all(&root).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let connection = subc_transport::ConnectionInfo {
        schema: subc_transport::SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![subc_transport::Endpoint {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        }],
        key: subc_transport::generate_key().unwrap(),
        daemon_id: subc_transport::generate_daemon_id().unwrap(),
        pid: std::process::id(),
        daemon_ver: "test".into(),
    };
    let path = root.join("connection.json");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap()
        .write_all(&serde_json::to_vec(&connection).unwrap())
        .unwrap();
    let daemon = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        authenticate_server(
            &mut stream,
            &connection.key,
            &connection.daemon_id,
            &connection.daemon_ver,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let route = read_frame(&mut stream).await.unwrap().unwrap();
        assert_eq!(route.header.channel, 0);
        let body: Value = serde_json::from_slice(&route.body).unwrap();
        assert_eq!(body["op"], "route.open");
        assert_eq!(
            body["target"],
            json!({"kind":"management_surface", "module_id":"claustrum"})
        );
        assert_eq!(
            body["consumer_identity"],
            json!({"module_id":"ck-motor", "launch_nonce":"test-launch-nonce"})
        );
        response(
            &mut stream,
            &route,
            json!({"op":"route.open", "route_channel":7, "route_epoch":1}),
        )
        .await;
        let request = read_frame(&mut stream).await.unwrap().unwrap();
        assert_eq!(request.header.channel, 7);
        assert_eq!(request.header.epoch, 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            json!({
                "method":"credential.account_token", "params":{"audience":"cortexkit-runners", "min_ttl_ms":30000}
            })
        );
        response(
            &mut stream,
            &request,
            super::tests::token("wire.fixture.jwt"),
        )
        .await;
        // Keep the authenticated peer alive until the consumer explicitly closes.
        while read_frame(&mut stream).await.ok().flatten().is_some() {}
    });
    let vault = Vault {
        connection: path,
        workspace: root.clone(),
        module_id: "ck-motor".into(),
    };
    let reply = tokio::time::timeout(Duration::from_secs(10), vault.request_with_nonce(json!({
        "method":"credential.account_token", "params":{"audience":"cortexkit-runners", "min_ttl_ms":30000}
    }), "test-launch-nonce")).await.unwrap().unwrap();
    assert_eq!(decode(reply).unwrap(), "wire.fixture.jwt");
    tokio::time::timeout(Duration::from_secs(2), daemon)
        .await
        .unwrap()
        .unwrap();
}
