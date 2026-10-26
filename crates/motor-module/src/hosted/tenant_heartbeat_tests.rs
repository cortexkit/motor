//! ck-motor's real hosted channel against the real tenant runner boundary.
//!
//! motor-host relays a hosted connection to the runner byte for byte after
//! prepending `verified_lease`, so this test writes that frame itself and then
//! hands the pipe to `Channel::from_hosted`, the constructor production uses.
//! That channel starts writing a heartbeat every second once hello completes.
//! The tenant runner used to refuse heartbeats as an unknown operation and
//! close the connection, so every hosted job lost its stream about a second
//! after hello and ended outcome-unknown.

use crate::transport::{Channel, Incoming};
use motor_protocol::*;
use sha2::Digest;
use std::{path::Path, process::Command, time::Duration};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A one-commit repository and a bundle of it, as ck-motor uploads it.
fn repository(scratch: &Path) -> (String, String, Vec<u8>) {
    let source = scratch.join("local/main");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q"]);
    std::fs::write(source.join("source"), "first\n").unwrap();
    git(&source, &["add", "."]);
    git(
        &source,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "snapshot",
        ],
    );
    let reference = format!("refs/ck-motor/{}/snapshot", Uuid::now_v7());
    git(&source, &["update-ref", &reference, "HEAD"]);
    let bundle = scratch.join("snapshot.bundle");
    git(
        &source,
        &["bundle", "create", bundle.to_str().unwrap(), &reference],
    );
    (
        git(&source, &["rev-parse", "HEAD"]),
        git(&source, &["rev-parse", "HEAD^{tree}"]),
        std::fs::read(bundle).unwrap(),
    )
}

#[tokio::test]
async fn hosted_channel_heartbeats_keep_a_tenant_job_streaming_to_its_terminal() {
    let scratch = cortexkit_test_support::ScratchDir::new("tenant-heartbeat");
    let remote = scratch.join("remote");
    let mut config = motor_runner::Config::new(scratch.join("runner-state"), "e2e");
    config.tenant_vm = true;
    config.free_space_floor_bytes = 0;
    config.max_weight = 2;
    config.workspace_roots = vec![remote.join("worktrees")];
    config.owned_workspace_roots = vec![remote.clone()];
    // No generated artifacts: an empty warm target needs no reflink fixture.
    config.base_commands = vec!["true".into()];
    let runner = motor_runner::Runner::open(config).unwrap();

    let (commit, tree, bundle) = repository(scratch.path());
    let id = Uuid::now_v7();
    let root = remote.join("source/main").to_string_lossy().into_owned();
    let workspace = remote.join("worktrees/a").to_string_lossy().into_owned();
    let run: Run = serde_json::from_value(serde_json::json!({
        "job_id": id, "workspace_key": workspace, "repository_root": root,
        "cwd": workspace, "command": "printf hosted; exit 7", "env": {},
        "weight_hint": 2, "timeout": 30, "queue_wait_limit_s": 300,
        "snapshot_commit": commit, "tree_hash": tree, "integration_tip": commit,
        "base_commit": commit, "siblings": [],
        "bundles": [{"index": 0, "repository": root, "length": bundle.len(),
            "sha256": format!("{:x}", sha2::Sha256::digest(&bundle))}],
    }))
    .unwrap();
    let lease = VerifiedLease {
        jti: id,
        purpose: LeasePurpose::Run,
        sub: "acct:test".into(),
        vcpus: 2,
        mem_mib: 16384,
        platform: if cfg!(target_arch = "aarch64") {
            "linux-arm64"
        } else {
            "linux-x64"
        }
        .into(),
        net: false,
        max_s: 30,
        exp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60,
    };

    let (mut host, guest) = tokio::io::duplex(1 << 20);
    let served = tokio::spawn(motor_runner::serve_tenant_connection(runner.clone(), guest));
    write_frame_async(&mut host, &Frame::VerifiedLease(lease))
        .await
        .unwrap();
    let mut channel = Channel::from_hosted(Box::new(host), false).await.unwrap();
    channel.send(Frame::Run(run)).await.unwrap();
    assert!(matches!(
        channel.next().await.unwrap(),
        Incoming::Frame(Frame::Accepted(_))
    ));
    // A real bundle upload over a WAN link outlasts several heartbeats. Let
    // two go out before the first chunk to stand in for a slow upload.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    channel
        .send(Frame::BundleChunk {
            index: 0,
            offset: 0,
            data: BytePayload(bundle),
        })
        .await
        .expect("the tenant runner closed the hosted connection during upload");
    let mut output = Vec::new();
    let terminal = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match channel.next().await {
                Ok(Incoming::Frame(Frame::Output(record))) => output.extend(record.bytes.0),
                Ok(Incoming::Frame(Frame::Terminal(terminal))) => break terminal,
                Ok(_) => {}
                Err(error) => panic!("hosted stream closed before its terminal: {error}"),
            }
        }
    })
    .await
    .expect("no terminal within 60 seconds");
    assert_eq!(terminal.outcome, Outcome::Exit { code: 7 }, "{terminal:?}");
    assert_eq!(String::from_utf8_lossy(&output), "hosted");
    drop(channel);
    served.await.unwrap().unwrap();
}
