use super::*;

fn job_runner(f: &Fixture) -> Runner {
    let mut config = f.config();
    config.manual_tick = true;
    config.tenant_vm = true;
    config.per_job = true;
    Runner::open(config).unwrap()
}

#[test]
fn job_vm_accepts_one_execution_and_workspace_even_after_completion() {
    let f = Fixture::new();
    let runner = job_runner(&f);
    let (first, _) = f.run("true");
    assert!(matches!(
        runner.accept(first.clone()).unwrap(),
        Admission::Accepted(_)
    ));
    assert!(matches!(
        runner.accept(first.clone()).unwrap(),
        Admission::Duplicate
    ));
    runner.cancel(first.job_id).unwrap();
    for same_workspace in [true, false] {
        let mut next = first.clone();
        next.job_id = Uuid::now_v7();
        if !same_workspace {
            next.workspace_key.push_str("-other");
        }
        assert!(matches!(
            runner.accept(next.clone()).unwrap(),
            Admission::Refused(RefusalReason::RequestRejected)
        ));
        let job = runner.core.inner.lock().unwrap().jobs[&next.job_id].clone();
        assert!(job.started_ms.is_none());
        assert!(matches!(
            job.terminal.unwrap().outcome,
            Outcome::RefusedBeforeStart {
                reason: RefusalReason::RequestRejected
            }
        ));
    }
    assert!(!runner.warm_builds_enabled("tip"));
}

#[test]
fn job_vm_preparation_cannot_select_a_second_workspace() {
    let f = Fixture::new();
    let runner = job_runner(&f);
    let (first, _) = f.run("true");
    runner
        .claim_prepare_workspace(&first.workspace_key)
        .unwrap();
    runner
        .claim_prepare_workspace(&first.workspace_key)
        .unwrap();
    assert!(runner
        .claim_prepare_workspace("/home/another-workspace")
        .is_err());
    let mut other = first.clone();
    other.workspace_key.push_str("-other");
    assert!(matches!(
        runner.accept(other).unwrap(),
        Admission::Refused(RefusalReason::RequestRejected)
    ));
}

#[test]
fn tenant_mode_still_accepts_multiple_jobs_and_workspaces() {
    let f = Fixture::new();
    let runner = open(&f);
    let (first, _) = f.run("true");
    let mut other = first.clone();
    other.job_id = Uuid::now_v7();
    other.workspace_key.push_str("-other");
    other.cwd = other.workspace_key.clone();
    for request in [first, other] {
        assert!(matches!(
            runner.accept(request).unwrap(),
            Admission::Accepted(_)
        ));
    }
}
