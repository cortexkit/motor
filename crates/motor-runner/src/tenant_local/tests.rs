use super::*;
use cortexkit_test_support::ScratchDir;
use motor_protocol::{
    read_frame_async, write_frame_async, Frame, LeasePurpose, Uuid, VerifiedLease,
};

fn owner() -> u32 {
    // SAFETY: geteuid has no arguments or preconditions.
    unsafe { libc::geteuid() }
}
struct SocketDirectory(std::path::PathBuf);
impl SocketDirectory {
    fn new() -> Self {
        // Process scratch includes a kernel start-time identity. Socket paths
        // need the shorter shared root to fit Darwin's sun_path limit.
        let root = cortexkit_test_support::scratch_root();
        let path = root.parent().unwrap().join(format!(
            "s{:x}-{:x}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for SocketDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn hello() -> Frame {
    Frame::Hello {
        return_paths: false,
        return_chunked: false,
        protocol_version: 1,
        started: true,
        default_branch_tip: false,
        gate_jobs: false,
        platforms: vec![],
    }
}
async fn next(socket: &mut UnixStream) -> Option<Frame> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match read_frame_async(socket).await.unwrap() {
                Some(Frame::Heartbeat { .. }) => {}
                frame => return frame,
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn local_tenant_sockets_only_replace_owned_dead_socket_nodes() {
    let socket_directory = SocketDirectory::new();
    let directory = socket_directory.0.clone();
    let path = directory.join("jobs.sock");
    std::fs::write(&path, "not a socket").unwrap();
    assert!(bind(&directory, "jobs.sock", owner()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"not a socket");
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("absent", &path).unwrap();
    assert!(bind(&directory, "jobs.sock", owner()).is_err());
    std::fs::remove_file(&path).unwrap();
    let listener = bind(&directory, "jobs.sock", owner()).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    assert!(bind(&directory, "jobs.sock", owner()).is_err());
    assert!(private_directory(&directory, owner() + 1).is_err());
    drop(listener);
    let _listener = bind(&directory, "jobs.sock", owner()).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(bind(&directory, "events.sock", owner()).is_err());
    assert!(trusted_ancestors(Path::new("relative")).is_err());
    let link = directory.join("link");
    std::os::unix::fs::symlink(&directory, &link).unwrap();
    assert!(private_directory(&link, owner()).is_err());
}

#[tokio::test]
async fn local_tenant_channels_require_lease_preserve_purpose_and_cancel_via_admin() {
    let scratch = ScratchDir::new("tenant-channels");
    let socket_directory = SocketDirectory::new();
    let directory = socket_directory.0.clone();
    let mut config = crate::Config::new(scratch.join("state"), "local-tenant");
    assert!(config.tenant_socket_dir.is_none());
    config.manual_tick = true;
    config.free_space_floor_bytes = 0;
    config.tenant_vm = true;
    let runner = Runner::open(config).unwrap();
    assert!(serve_tenant_local(runner.clone(), &directory)
        .await
        .is_err());
    let jobs = bind(&directory, "jobs.sock", owner()).unwrap();
    let events = bind(&directory, "events.sock", owner()).unwrap();
    let admin = bind(&directory, "admin.sock", owner()).unwrap();
    let task = tokio::spawn(serve_listeners(runner.clone(), jobs, events, admin));
    let mut client = UnixStream::connect(directory.join("jobs.sock"))
        .await
        .unwrap();
    write_frame_async(&mut client, &hello()).await.unwrap();
    assert!(next(&mut client).await.is_none());
    let id = Uuid::now_v7();
    let mut client = UnixStream::connect(directory.join("jobs.sock"))
        .await
        .unwrap();
    write_frame_async(
        &mut client,
        &Frame::VerifiedLease(VerifiedLease {
            jti: id,
            purpose: LeasePurpose::Attach,
            sub: "acct:test".into(),
            vcpus: 1,
            mem_mib: 16384,
            platform: "linux-x64".into(),
            net: false,
            max_s: 30,
            exp: u64::MAX / 1000,
        }),
    )
    .await
    .unwrap();
    write_frame_async(&mut client, &hello()).await.unwrap();
    assert!(matches!(next(&mut client).await, Some(Frame::Hello { .. })));
    write_frame_async(&mut client, &Frame::Status)
        .await
        .unwrap();
    assert!(next(&mut client).await.is_none());
    assert!(!runner.has_job_record(id));
    assert!(dispatch(runner.clone(), TenantAgentRequest::ArgsStatus)
        .await
        .is_err());
    assert_eq!(
        dispatch(runner.clone(), TenantAgentRequest::Cancel { job_id: id })
            .await
            .unwrap(),
        json!({"ok":true})
    );
    assert!(runner.has_job_record(id));
    let mut event_client = UnixStream::connect(directory.join("events.sock"))
        .await
        .unwrap();
    let len = event_client.read_u32().await.unwrap();
    let mut bytes = vec![0; len as usize];
    event_client.read_exact(&mut bytes).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["type"],
        "event_replay"
    );
    let mut admin_client = UnixStream::connect(directory.join("admin.sock"))
        .await
        .unwrap();
    let request = br#"{"op":"resume"}"#;
    admin_client.write_u32(request.len() as u32).await.unwrap();
    admin_client.write_all(request).await.unwrap();
    let len = admin_client.read_u32().await.unwrap();
    let mut reply = vec![0; len as usize];
    admin_client.read_exact(&mut reply).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&reply).unwrap(),
        json!({"ok":true})
    );
    drop(client);
    drop(event_client);
    drop(admin_client);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}
