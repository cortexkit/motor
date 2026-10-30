use super::{progress, Motor};
use motor_protocol::{Frame, JobStatusReply, Platform, ProgressPhase};
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

pub(super) struct Submission {
    motor: Motor,
    id: Uuid,
}

impl Drop for Submission {
    fn drop(&mut self) {
        self.motor.health_progress.lock().unwrap().remove(&self.id);
    }
}

fn platform_name(platform: &Platform) -> &'static str {
    match platform {
        Platform::Windows => "windows",
        Platform::Macos => "macos",
        _ => "linux",
    }
}

fn job_state(reply: &JobStatusReply) -> Option<&'static str> {
    if reply.terminal.is_some() || reply.state == "terminal" {
        None
    } else if reply.state == "running" {
        Some("running")
    } else if reply.waiting_on.as_deref().is_some_and(|waiting| {
        matches!(
            waiting,
            "permits"
                | "gate job ahead"
                | "cache fill ahead needs more free permits"
                | "older job ahead needs more free permits"
        )
    }) {
        Some("waiting_for_capacity")
    } else if matches!(reply.state.as_str(), "queued" | "uploading" | "starting") {
        // Workspace, upload and setup waits are not waits for remote capacity.
        None
    } else {
        Some("unknown")
    }
}

impl Motor {
    pub(super) fn observe_health_submission(
        &self,
        id: Uuid,
        platform: Platform,
        reporter: progress::Reporter,
    ) -> Submission {
        self.health_progress
            .lock()
            .unwrap()
            .insert(id, (platform, reporter));
        Submission {
            motor: self.clone(),
            id,
        }
    }

    pub(crate) async fn macos_health(&self) -> Option<Value> {
        self.config.macos_runner.as_ref()?;
        let status = tokio::time::timeout(
            super::WINDOWS_PROBE_TIMEOUT,
            self.direct_runner_status_for(Some(&Platform::Macos)),
        )
        .await
        .ok()
        .and_then(Result::ok);
        Some(
            json!({"reachable": status.is_some(), "busy": status.as_ref().is_some_and(|status| {
                status.status.queue_depth > 0 || status.health.as_ref().is_some_and(|health| {
                    matches!((health.permits_held, health.permits_cap), (Some(held), Some(cap)) if held >= cap)
                })
            })}),
        )
    }

    pub(crate) async fn machine_jobs_health(&self) -> Value {
        let mut jobs = json!({});
        for platform in ["linux", "windows", "macos"] {
            jobs[platform] = json!({"running": 0, "waiting_for_capacity": 0, "unknown": 0});
        }
        let active: std::collections::HashMap<_, _> = self
            .health_progress
            .lock()
            .unwrap()
            .iter()
            .map(|(id, (platform, reporter))| (*id, (platform.clone(), reporter.current_phase())))
            .collect();
        let mut probes = tokio::task::JoinSet::new();
        let candidates = self.storage.health_jobs();
        for batch in candidates.chunks(16) {
            for (id, job) in batch {
                let platform = active
                    .get(id)
                    .map(|(platform, _)| platform_name(platform))
                    .unwrap_or_else(|| {
                        if job.windows_runner.is_some()
                            || job.runner_instance.as_ref().is_some_and(|instance| {
                                self.config.windows_runner_instance.as_ref() == Some(instance)
                            })
                        {
                            "windows"
                        } else if job.runner_instance.as_ref().is_some_and(|instance| {
                            self.config
                                .macos_runner
                                .as_ref()
                                .is_some_and(|runner| &runner.instance == instance)
                        }) {
                            "macos"
                        } else {
                            "linux"
                        }
                    });
                if let Some((_, Some(phase))) = active.get(id) {
                    if *phase == ProgressPhase::PlacementWait {
                        let count = jobs[platform]["waiting_for_capacity"].as_u64().unwrap();
                        jobs[platform]["waiting_for_capacity"] = json!(count + 1);
                    }
                    continue;
                }
                let motor = self.clone();
                let id = *id;
                probes.spawn(async move {
                    // Use a status frame directly so health neither renews job retention
                    // nor reports final usage and releases the broker's reservation.
                    let probe = async {
                        let mut channel = motor.open_job_channel(id).await.ok()?;
                        let runner_id = motor.build_server_id(id);
                        channel
                            .send(Frame::JobStatus { job_id: runner_id })
                            .await
                            .ok()?;
                        match channel.next().await.ok()? {
                            crate::transport::Incoming::Frame(Frame::JobStatusReply(reply))
                                if reply.job_id == runner_id =>
                            {
                                Some(reply)
                            }
                            _ => None,
                        }
                    };
                    let reply = tokio::time::timeout(Duration::from_secs(2), probe)
                        .await
                        .ok()
                        .flatten();
                    (platform, reply)
                });
            }
            while let Some(result) = probes.join_next().await {
                if let Ok((platform, reply)) = result {
                    let state = reply.as_ref().map_or(Some("unknown"), job_state);
                    if let Some(state) = state {
                        let count = jobs[platform][state].as_u64().unwrap();
                        jobs[platform][state] = json!(count + 1);
                    }
                }
            }
        }
        jobs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use motor_protocol::{
        read_frame_async, write_frame_async, Outcome, Ran, TerminalRecord, PROTOCOL_VERSION,
    };
    use std::{
        collections::HashMap,
        io,
        sync::{Arc, Mutex},
    };

    struct StatusPeer {
        replies: HashMap<Uuid, JobStatusReply>,
        visits: Arc<Mutex<Vec<Uuid>>>,
    }
    impl crate::Connector for StatusPeer {
        fn connect(
            &self,
            id: Uuid,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = io::Result<Box<dyn crate::Duplex>>> + Send + '_>,
        > {
            self.visits.lock().unwrap().push(id);
            let reply = self.replies.get(&id).cloned().unwrap();
            Box::pin(async move {
                let (client, mut peer) = tokio::io::duplex(65536);
                tokio::spawn(async move {
                    assert!(matches!(
                        read_frame_async(&mut peer).await.unwrap(),
                        Some(Frame::Hello { .. })
                    ));
                    write_frame_async(
                        &mut peer,
                        &Frame::Hello {
                            protocol_version: PROTOCOL_VERSION,
                            return_paths: false,
                            return_chunked: false,
                            sibling_paths: false,
                            started: false,
                            default_branch_tip: false,
                            gate_jobs: false,
                            platforms: Vec::new(),
                        },
                    )
                    .await
                    .unwrap();
                    loop {
                        match read_frame_async(&mut peer).await.unwrap() {
                            Some(Frame::Heartbeat { .. }) => continue,
                            Some(Frame::JobStatus { job_id }) => {
                                assert_eq!(job_id, id);
                                break;
                            }
                            other => panic!("unexpected health probe: {other:?}"),
                        }
                    }
                    write_frame_async(&mut peer, &Frame::JobStatusReply(reply))
                        .await
                        .unwrap();
                });
                Ok(Box::new(client) as Box<dyn crate::Duplex>)
            })
        }
    }
    fn reply(id: Uuid, state: &str, waiting: Option<&str>) -> JobStatusReply {
        JobStatusReply {
            job_id: id,
            state: state.into(),
            waiting_on: waiting.map(str::to_owned),
            terminal: None,
            unit: None,
            unit_alive: None,
            queue_wait_ms: Some(95 * 60_000),
        }
    }

    #[test]
    fn job_counts_distinguish_capacity_from_workspace_and_historical_waits() {
        let mut reply = reply(Uuid::nil(), "queued", Some("permits"));
        assert_eq!(job_state(&reply), Some("waiting_for_capacity"));
        reply.waiting_on = Some("workspace".into());
        assert_eq!(job_state(&reply), None);
        reply.state = "running".into();
        reply.waiting_on = None;
        assert_eq!(job_state(&reply), Some("running"));
        reply.state = "terminal".into();
        assert_eq!(job_state(&reply), None);
    }

    #[tokio::test]
    async fn machine_counts_only_its_jobs_per_platform_and_live_capacity_waits() {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-machine-health");
        let visits = Arc::new(Mutex::new(Vec::new()));
        let linux = Uuid::now_v7();
        let windows = Uuid::now_v7();
        let macos = Uuid::now_v7();
        let placement = Uuid::now_v7();
        let workspace = Uuid::now_v7();
        let finished = Uuid::now_v7();
        let unknown = Uuid::now_v7();
        let foreign = Uuid::now_v7();
        let replies = [
            reply(linux, "running", None),
            reply(windows, "queued", Some("gate job ahead")),
            reply(macos, "running", None),
            reply(workspace, "queued", Some("workspace")),
            reply(unknown, "not_found", None),
            reply(foreign, "running", None),
        ]
        .into_iter()
        .map(|reply| (reply.job_id, reply))
        .collect();
        let motor = Motor::with_connector(
            crate::Config::default(),
            &scratch,
            Arc::new(StatusPeer {
                replies,
                visits: visits.clone(),
            }),
        )
        .unwrap();
        let mut guards = Vec::new();
        for (id, platform) in [
            (linux, Platform::Linux),
            (windows, Platform::Windows),
            (macos, Platform::Macos),
            (placement, Platform::Linux),
            (workspace, Platform::Linux),
            (finished, Platform::Linux),
            (unknown, Platform::Linux),
        ] {
            motor.storage.create(id, 60, None, None, false).unwrap();
            let reporter = progress::Reporter::new(false, 2);
            if id == placement {
                reporter.phase(ProgressPhase::PlacementWait);
            }
            guards.push(motor.observe_health_submission(id, platform, reporter));
        }
        motor
            .storage
            .local_terminal(
                TerminalRecord::new(finished, Outcome::Exit { code: 0 }, 0, 0, 0)
                    .with_ran(Ran::Remote),
            )
            .unwrap();
        let counts = motor.machine_jobs_health().await;
        assert_eq!(
            counts,
            json!({
                "linux": {"running": 1, "waiting_for_capacity": 1, "unknown": 1},
                "windows": {"running": 0, "waiting_for_capacity": 1, "unknown": 0},
                "macos": {"running": 1, "waiting_for_capacity": 0, "unknown": 0},
            })
        );
        let visits = visits.lock().unwrap();
        assert_eq!(visits.len(), 5);
        assert!(!visits.contains(&foreign));
        assert!(!visits.contains(&placement));
        assert!(!visits.contains(&finished));
        drop(visits);
        drop(guards);
        assert!(motor.health_progress.lock().unwrap().is_empty());
    }
}
