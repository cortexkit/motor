use super::*;
use crate::{
    agent_route::{agent_stream, take_return_request},
    hosted::{
        boat_tests::committed_repo,
        tests::harness::{Behavior, Harness, NoSsh},
    },
    wsl::{Checks, Probe},
};
use std::sync::atomic::{AtomicUsize, Ordering};

fn aliases(path: &Path) -> io::Result<PathBuf> {
    match path.to_str().unwrap() {
        "/home/u/link" => Ok("/mnt/c/project".into()),
        "/mnt/c/link" => Ok("/home/u/project".into()),
        _ => std::fs::canonicalize(path),
    }
}

#[tokio::test]
async fn wsl_agent_exec_run_refuses_every_field_before_all_route_states() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    for route in ["off", "hosted", "fleet", "repository", "forced"] {
        for probe in [
            Probe::Interop,
            Probe::InteropLate,
            Probe::DistroName,
            Probe::Release,
        ] {
            let mut config = h.config();
            config.hosted_runners = matches!(route, "hosted" | "fleet");
            config.hosted_fleet_routing = route == "fleet";
            config.spill_to_hosted = true;
            config.spill_after_s = 0;
            if route == "repository" {
                config.hosted_repositories = vec![repo.display().to_string()];
            }
            let state = scratch.0.join(format!("wsl-{route}-{probe:?}"));
            let mut motor = Motor::with_connector(config, &state, peer.clone()).unwrap();
            motor.wsl = Checks::test(probe).with_canonicalize(aliases);
            for path in ["/mnt/c", "/mnt/C/x", "/home/u/link", "/mnt/c/link"] {
                for field in ["workspace_key", "repository_root", "cwd", "sibling"] {
                    let root = repo.display().to_string();
                    let mut request = RunRequest::new(&root, &root, &root, "true").with_timeout(60);
                    match field {
                        "workspace_key" => request.workspace_key = path.into(),
                        "repository_root" => request.repository_root = path.into(),
                        "cwd" => request.cwd = path.into(),
                        "sibling" => request.siblings = vec![path.into()],
                        _ => unreachable!(),
                    }
                    let stream = if route == "forced" {
                        motor
                            .run_direct_hosted(request, None, Default::default())
                            .unwrap()
                    } else {
                        // Use the same request parsing and agent entry point as exec.run.
                        let mut request =
                            serde_json::from_value(serde_json::to_value(request).unwrap()).unwrap();
                        let returns = take_return_request(&mut request).unwrap();
                        motor.run_agent(request, None, returns).unwrap()
                    };
                    let id = stream.job_id;
                    assert!(stream.hosted_state().is_none(), "{route} {probe:?} {field}");
                    let records = agent_stream(stream, 0).await;
                    assert_eq!(records.len(), 1, "{route} {probe:?} {field}");
                    let StreamRecord::Terminal(terminal) = &records[0] else {
                        panic!("expected only a refusal terminal: {records:?}");
                    };
                    assert_eq!(
                        terminal.outcome,
                        Outcome::RefusedBeforeStart {
                            reason: RefusalReason::WorkspaceKeyRejected,
                        }
                    );
                    assert_eq!(terminal.refusal_detail(), Some(format!("ck-motor: this checkout is on a Windows drive ({path}). Under WSL, move it into the WSL filesystem (for example under ~/) and run ck motor there.").as_str()));
                    assert_eq!(terminal.ran, Some(Ran::None));
                    assert!(terminal.tree_hash.is_none());
                    assert_eq!(
                        (
                            terminal.wall_ms,
                            terminal.queue_wait_ms,
                            terminal.bundle_bytes
                        ),
                        (0, 0, 0)
                    );
                    assert!(motor.hosted_state(id).is_none());
                    assert!(!state.join("spools").exists());
                }
            }
        }
    }
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
    assert!(h.requests.lock().unwrap().is_empty());
    assert!(h.frames.lock().unwrap().is_empty());
}

#[tokio::test]
async fn wsl_prepare_refuses_every_field_before_hosted_route_and_snapshot() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    for hosted in [false, true] {
        for probe in [
            Probe::Interop,
            Probe::InteropLate,
            Probe::DistroName,
            Probe::Release,
        ] {
            let mut config = h.config();
            config.hosted_runners = hosted;
            let state = scratch.0.join(format!("wsl-prepare-{hosted}-{probe:?}"));
            let mut motor = Motor::with_connector(config, &state, peer.clone()).unwrap();
            motor.wsl = Checks::test(probe).with_canonicalize(aliases);
            for path in ["/mnt/c", "/mnt/C/x", "/home/u/link", "/mnt/c/link"] {
                for field in ["workspace_key", "repository_root", "sibling"] {
                    let root = repo.display().to_string();
                    let mut request = PrepareRequest::new(&root, &root, "HEAD");
                    match field {
                        "workspace_key" => request.workspace_key = path.into(),
                        "repository_root" => request.repository_root = path.into(),
                        "sibling" => request.siblings = vec![path.into()],
                        _ => unreachable!(),
                    }
                    let reply = motor.prepare(request).await.unwrap();
                    assert_eq!(
                        reply.outcome,
                        PrepareOutcome::RefusedBeforeStart {
                            reason: RefusalReason::WorkspaceKeyRejected,
                        }
                    );
                    assert_eq!(reply.refusal_detail(), Some(format!("ck-motor: this checkout is on a Windows drive ({path}). Under WSL, move it into the WSL filesystem (for example under ~/) and run ck motor there.").as_str()));
                    assert!(motor.hosted_state(reply.transfer_id).is_none());
                    assert!(!state.join("spools").exists());
                }
            }
        }
    }
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
    assert!(h.requests.lock().unwrap().is_empty());
    assert!(h.frames.lock().unwrap().is_empty());
}

#[tokio::test]
async fn wsl_non_drive_and_non_wsl_requests_keep_normal_validation_and_routing() {
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let peer = Arc::new(NoSsh(AtomicUsize::new(0)));
    for route in ["off", "hosted", "fleet", "repository", "forced"] {
        let mut config = h.config();
        config.hosted_runners = matches!(route, "hosted" | "fleet");
        config.hosted_fleet_routing = route == "fleet";
        if route == "repository" {
            config.hosted_repositories = vec![repo.display().to_string()];
        }
        let state = scratch.0.join(format!("wsl-negative-{route}"));
        let mut motor = Motor::with_connector(config, &state, peer.clone()).unwrap();
        for (path, probe) in [
            ("/mnt/c", Probe::None),
            ("/mnt/data", Probe::Interop),
            ("/mnt/cc", Probe::InteropLate),
            ("/mnt/c2", Probe::Release),
        ] {
            motor.wsl = Checks::test(probe);
            let root = repo.display().to_string();
            let mut request = RunRequest::new(path, &root, path, "true").with_timeout(60);
            let stream = if route == "forced" {
                motor
                    .run_direct_hosted(request, None, Default::default())
                    .unwrap()
            } else {
                let returns = take_return_request(&mut request).unwrap();
                motor.run_agent(request, None, returns).unwrap()
            };
            let id = stream.job_id;
            let records = agent_stream(stream, 0).await;
            let StreamRecord::Terminal(terminal) = records.last().unwrap() else {
                panic!("expected a terminal");
            };
            // These are not checkouts. Ordinary validation still refuses them,
            // but the WSL guard must leave the normal routing decision intact.
            assert!(matches!(
                terminal.outcome,
                Outcome::RefusedBeforeStart { .. }
            ));
            assert!(!terminal.refusal_detail().unwrap().contains("Under WSL"));
            assert_eq!(
                motor.hosted_state(id).is_some(),
                matches!(route, "hosted" | "repository" | "forced")
            );

            let reply = motor
                .prepare(PrepareRequest::new(path, &root, "HEAD"))
                .await
                .unwrap();
            assert!(!reply.refusal_detail().unwrap().contains("Under WSL"));
            assert_eq!(
                motor.hosted_state(reply.transfer_id).is_some(),
                matches!(route, "hosted" | "fleet")
            );
            if matches!(route, "hosted" | "fleet") {
                assert_eq!(
                    reply.refusal_detail(),
                    Some("hosted_prepare_unsupported: hosted_no_fallback")
                );
            } else {
                assert_eq!(
                    reply.outcome,
                    PrepareOutcome::RefusedBeforeStart {
                        reason: RefusalReason::WorkspaceKeyRejected,
                    }
                );
            }
        }
    }
    assert_eq!(peer.0.load(Ordering::SeqCst), 0);
    assert!(h.requests.lock().unwrap().is_empty());
    assert!(h.frames.lock().unwrap().is_empty());
}
