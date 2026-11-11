use super::*;

#[test]
fn block_reaches_each_running_job_vm_and_reports_unknown_runner_controls() {
    let (runtime, fake, _storage) = fixture();
    for job in [JOB, JOB2] {
        let mut c = claims(job);
        c.workspace_key_hash = Some(motor_lease::base64url(if job == JOB {
            &[5; 32]
        } else {
            &[6; 32]
        }));
        claim(&runtime, &c);
        runtime
            .host
            .lock()
            .unwrap()
            .snapshot
            .per_job
            .executions
            .get_mut(job)
            .unwrap()
            .boot_started = true;
    }
    *fake.write_block_reply.lock().unwrap() = Some(json!({"ok":false,"error":"unknown_frame"}));
    assert_eq!(
        runtime
            .host_admin(&json!({"op":"set_remote_cache_write_block","blocked":true}))
            .unwrap()["not_acknowledged"],
        json!([JOB, JOB2])
    );
    assert_eq!(fake.admin_requests.lock().unwrap().len(), 2);
    assert!(
        runtime
            .host
            .lock()
            .unwrap()
            .snapshot
            .remote_cache_write_blocked
    );
    *fake.write_block_reply.lock().unwrap() = None;
    assert_eq!(
        runtime
            .host_admin(&json!({"op":"set_remote_cache_write_block","blocked":false}))
            .unwrap()["not_acknowledged"],
        json!([])
    );
    assert_eq!(fake.admin_requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn boot_while_blocked_resends_deny_before_job_lease() {
    let (runtime, fake, storage) = fixture();
    runtime
        .host_admin(&json!({"op":"set_remote_cache_write_block","blocked":true}))
        .unwrap();
    *fake.write_block_reply.lock().unwrap() = Some(json!({"ok":false,"error":"unsupported"}));
    let c = claims(JOB);
    let id = claim(&runtime, &c);
    existing_workspace(&runtime, JOB);
    let (mut client, task) = start(runtime.clone(), c, id).await;
    next_type(&mut client, "lease_ok").await;
    write_frame(&mut client, &hello()).await.unwrap();
    next_type(&mut client, "hello").await;
    wait_for_runner_hello(&runtime, JOB).await;
    assert_eq!(
        fake.admin_requests.lock().unwrap().as_slice(),
        [(
            JOB.into(),
            json!({"op":"set_remote_cache_write_block","blocked":true})
        )]
    );
    assert_eq!(fake.inputs.lock().unwrap().len(), 1);
    write_frame(&mut client, &run(JOB)).await.unwrap();
    next_type(&mut client, "accepted").await;
    next_type(&mut client, "output").await;
    drop(client);
    task.await.unwrap().unwrap();
    runtime.stop_job(JOB, true, false).unwrap();
    drop(storage);
}
