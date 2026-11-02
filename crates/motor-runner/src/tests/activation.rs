use super::*;

fn fill(runner: &Runner, f: &Fixture, running: bool) -> Uuid {
    let (mut request, _) = f.run("true");
    request.workspace_key = format!("base:{}", f.key);
    let id = request.job_id;
    let mut job = Job::new(id, Some(request), runner.now(), 1, 1);
    job.base = true;
    if running {
        job.state = JobState::Running;
    }
    runner.save(&job).unwrap();
    let mut inner = runner.core.inner.lock().unwrap();
    inner.jobs.insert(id, job);
    if running {
        inner.writer = Some(id);
        inner.finalizing.insert(id);
    }
    id
}

#[tokio::test]
async fn activation_drain_ignores_queued_and_running_fills_but_not_caller_work() {
    let f = Fixture::new();
    let runner = manual_tick_runner(&f, 4);
    let queued = fill(&runner, &f, false);
    let running = fill(&runner, &f, true);
    assert_eq!(runner.status().queue_depth, 1);
    assert_eq!(runner.status().running_jobs[0].job_id, running);
    assert_eq!(runner.runner_status().health.unwrap().caller_work, Some(0));

    // Deployment still waits for fills; only activation may abandon them.
    let waiter = runner.begin_drain_request(Duration::from_secs(30), false);
    assert_eq!(runner.drain_reply_for(&waiter), None);
    runner.resume();

    let (socket, server) = listener(&f, &runner);
    let mut channel = connect(&socket).await;
    write_frame_async(&mut channel, &Frame::DrainForActivation)
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), read_next(&mut channel))
            .await
            .unwrap(),
        Frame::DrainReply {
            timed_out: false,
            restart_recoverable_jobs: Some(0)
        }
    );
    let (request, _) = f.run("true");
    assert!(matches!(
        runner.accept(request).unwrap(),
        Admission::Refused(RefusalReason::RunnerDraining)
    ));
    runner.resume();

    let (request, _) = f.run("true");
    let caller = Job::new(request.job_id, Some(request), runner.now(), 2, 1);
    runner.save(&caller).unwrap();
    runner
        .core
        .inner
        .lock()
        .unwrap()
        .jobs
        .insert(caller.id, caller);
    assert!(runner.runner_status().health.unwrap().caller_work.unwrap() > 0);
    let mut channel = connect(&socket).await;
    write_frame_async(&mut channel, &Frame::DrainForActivation)
        .await
        .unwrap();
    assert_eq!(
        read_next(&mut channel).await,
        Frame::DrainReply {
            timed_out: true,
            restart_recoverable_jobs: Some(0)
        }
    );
    runner.resume();
    assert!(runner.core.inner.lock().unwrap().jobs[&queued].pending());
    server.abort();
    let mut inner = runner.core.inner.lock().unwrap();
    inner.finalizing.clear();
    inner.writer = None;
    for job in inner.jobs.values_mut() {
        job.terminal(Outcome::Cancelled, runner.now());
        runner.save(job).unwrap();
    }
}

#[test]
fn activation_status_is_additive_and_unknown_lifecycle_owners_fail_closed() {
    let f = Fixture::new();
    let runner = manual_tick_runner(&f, 4);
    let id = Uuid::now_v7();
    for kind in ["starting", "finalizing", "writer", "admission_save"] {
        {
            let mut inner = runner.core.inner.lock().unwrap();
            match kind {
                "starting" => {
                    inner.starting.insert(id);
                }
                "finalizing" => {
                    inner.finalizing.insert(id);
                }
                "writer" => inner.writer = Some(id),
                "admission_save" => {
                    inner.admission_saves.insert(id, f.key.clone());
                }
                _ => unreachable!(),
            }
        }
        assert!(
            runner.runner_status().health.unwrap().caller_work.unwrap() > 0,
            "{kind}"
        );
        assert!(runner.drain_for_activation() > 0, "{kind}");
        runner.resume();
        let mut inner = runner.core.inner.lock().unwrap();
        inner.starting.clear();
        inner.finalizing.clear();
        inner.writer = None;
        inner.admission_saves.clear();
    }
    let mut value = serde_json::to_value(runner.runner_status()).unwrap();
    assert_eq!(value["health"]["caller_work"], 0);
    value["health"]
        .as_object_mut()
        .unwrap()
        .remove("caller_work");
    let old: motor_protocol::RunnerStatusReply = serde_json::from_value(value).unwrap();
    assert_eq!(old.health.unwrap().caller_work, None);
}
