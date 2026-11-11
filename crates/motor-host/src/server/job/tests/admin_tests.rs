use super::*;

fn forwarded(request: Value) {
    let (runtime, fake, _) = fixture();
    claim(&runtime, &claims(JOB));
    assert_eq!(runtime.admin(JOB, &request).unwrap(), json!({"ok":true}));
    assert_eq!(
        *fake.admin_requests.lock().unwrap(),
        vec![(JOB.into(), request)]
    );
}

#[test]
fn local_admin_status_targets_job_vm() {
    forwarded(json!({"op":"status"}));
}

#[test]
fn local_admin_cancel_targets_only_selected_job() {
    forwarded(json!({"op":"cancel","job_id":JOB}));
}

#[test]
fn local_admin_runner_log_targets_job_vm() {
    forwarded(json!({"op":"diagnostics","since_unix_s":1,"until_unix_s":2,"jti":JOB}));
    forwarded(json!({"op":"diagnostics","since_unix_s":1}));
}

#[test]
fn local_admin_refuses_mismatched_ids_unknown_execution_and_tenant_only_ops() {
    let (runtime, fake, _) = fixture();
    claim(&runtime, &claims(JOB));
    assert!(runtime.admin(JOB2, &json!({"op":"status"})).is_err());
    for request in [
        json!({"op":"cancel","job_id":JOB2}),
        json!({"op":"diagnostics","jti":JOB2}),
        json!({"op":"drain"}),
        json!({"op":"resume"}),
        json!({"op":"attach_tenant"}),
        json!({"op":"status","extra":true}),
    ] {
        assert!(runtime.admin(JOB, &request).is_err(), "{request}");
    }
    assert!(runtime
        .admin("not-a-job-id", &json!({"op":"status"}))
        .is_err());
    assert!(fake.admin_requests.lock().unwrap().is_empty());
}
