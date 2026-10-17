use super::*;
use crate::network::{JobNetwork, Phase};

#[cfg(target_os = "linux")]
#[path = "network_operator.rs"]
mod operator;

#[cfg(target_os = "linux")]
pub(crate) async fn operator_case(mode: &str) {
    if mode == "restart" {
        operator::restart().await;
    } else if mode == "measurement" {
        for _ in 0..10 {
            operator::case("public").await;
        }
        operator::case("measure").await;
    } else {
        operator::case(mode).await;
    }
}

#[tokio::test]
async fn network_admission_refuses_before_upload_with_missing_prerequisite() {
    for case in ["unset", "probe", "unisolated", "windows", "unknown"] {
        let f = Fixture::new();
        let mut config = if case == "windows" {
            windows_instance::config(&f)
        } else {
            f.config()
        };
        config.disable_warm_builds = true;
        if case != "unset" {
            config.job_network = Some(JobNetwork::Veth);
        }
        if case == "probe" {
            config.job_network_hook = Some(Arc::new(|_| {
                Err(io::Error::other("motor_net table missing"))
            }));
        } else if matches!(case, "windows" | "unknown") {
            config.job_network_hook = Some(Arc::new(|_| Ok(())));
        }
        // The Windows case exercises admission only, never a Windows VM launch.
        let runner = Runner::open_with_backend(config, Box::new(backend::HostBackend)).unwrap();
        let (socket, server) = listener(&f, &runner);
        let (mut request, _) = f.run("touch never-started");
        request.network = Some(if case == "unknown" {
            Network::Unknown("other".into())
        } else {
            Network::Outbound
        });
        if case == "windows" {
            request.platform = Some(Platform::Windows);
        }
        let mut stream = if case == "windows" {
            let mut stream = UnixStream::connect(&*socket).await.unwrap();
            write_frame_async(
                &mut stream,
                &Frame::Hello {
                    return_paths: false,
                    protocol_version: PROTOCOL_VERSION,
                    started: true,
                    default_branch_tip: false,
                    gate_jobs: false,
                    platforms: Vec::new(),
                },
            )
            .await
            .unwrap();
            assert!(
                matches!(read_next(&mut stream).await, Frame::Hello { platforms, .. } if platforms == vec!["windows"])
            );
            stream
        } else {
            connect(&socket).await
        };
        write_frame_async(&mut stream, &Frame::Run(request.clone()))
            .await
            .unwrap();
        let Frame::Refusal(refusal) = read_next(&mut stream).await else {
            panic!("{case}: must refuse before upload")
        };
        assert_eq!(refusal.reason, RefusalReason::NetworkUnsupported, "{case}");
        let expected = match case {
            "unset" => "--job-network",
            "probe" => "motor_net table missing",
            "unisolated" => "isolate_jobs",
            "windows" => "Windows",
            _ => "unknown",
        };
        assert!(
            refusal.detail.as_deref().unwrap().contains(expected),
            "{case}: {refusal:?}"
        );
        assert!(
            !runner.spool_dir(request.job_id).exists(),
            "{case}: no upload spool"
        );
        let job: Job = read_json(&runner.record_path(request.job_id)).unwrap();
        assert!(job.process.is_none() && job.started_ms.is_none());
        assert_eq!(job.terminal.unwrap().ran, Some(Ran::None));
        assert!(!Path::new(&request.workspace_key)
            .join("never-started")
            .exists());
        server.abort();
    }
}

#[tokio::test]
async fn network_hook_grants_only_opted_in_runs_and_reaches_terminal() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    config.job_network = Some(JobNetwork::Veth);
    let attached = Arc::new(Mutex::new(Vec::new()));
    let seen = attached.clone();
    config.job_network_hook = Some(Arc::new(move |phase| {
        if let Phase::Attach(id) = phase {
            seen.lock().unwrap().push(id);
        }
        Ok(())
    }));
    // The hook overrides isolate_jobs=false without granting host networking.
    let runner = Runner::open(config).unwrap();
    let (socket, server) = listener(&f, &runner);
    for network in [Some(Network::Outbound), None] {
        let (mut request, bytes) = f.run("true");
        request.network = network.clone();
        let mut stream = connect(&socket).await;
        write_frame_async(&mut stream, &Frame::Run(request.clone()))
            .await
            .unwrap();
        let Frame::Accepted(accepted) = read_next(&mut stream).await else {
            panic!("expected grant")
        };
        assert_eq!(accepted.network(), network.as_ref());
        chunks(&mut stream, &bytes).await;
        assert_eq!(
            wire_terminal(&mut stream).await.outcome,
            Outcome::Exit { code: 0 }
        );
        if network.is_some() {
            assert_eq!(*attached.lock().unwrap(), vec![request.job_id]);
        } else {
            assert_eq!(
                attached.lock().unwrap().len(),
                1,
                "offline job must not attach"
            );
        }
    }
    server.abort();
}

#[tokio::test]
async fn network_attach_failure_refuses_before_gate_with_network_cause() {
    let f = Fixture::new();
    let mut config = f.config();
    config.disable_warm_builds = true;
    config.job_network = Some(JobNetwork::Veth);
    config.job_network_hook = Some(Arc::new(|phase| match phase {
        Phase::Probe => Ok(()),
        Phase::Attach(_) => Err(io::Error::other(
            "motor_net table unloaded during network attach",
        )),
    }));
    let runner = Runner::open(config).unwrap();
    let (mut request, bytes) = f.run("touch never-started");
    request.network = Some(Network::Outbound);
    f.submit(&runner, &request, &bytes);
    let terminal = terminal(&runner, request.job_id).await;
    let Outcome::RefusedBeforeStart { ref reason } = terminal.outcome else {
        panic!("{terminal:?}")
    };
    assert_eq!(*reason, RefusalReason::WorkspaceSetupFailed);
    assert!(terminal
        .refusal_detail()
        .unwrap()
        .contains("motor_net table unloaded"));
    assert_eq!(terminal.ran, Some(Ran::None));
    assert!(!Path::new(&request.workspace_key)
        .join("never-started")
        .exists());
    assert!(!runner
        .replay(request.job_id, 0)
        .unwrap()
        .iter()
        .any(|f| matches!(f, Frame::Started { .. })));
}
