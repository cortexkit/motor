use super::*;
use motor_protocol::{MirrorRefReport, RepositoryCandidates};

fn active(f: &mut Fixture) -> Claims {
    let c = f.claims();
    let mut host = f.runtime.host.lock().unwrap();
    host.claim(c.clone(), unix_ms()).unwrap();
    host.begin_job_boot(&c.jti).unwrap().unwrap();
    c
}
fn reply(f: &Fixture, c: &Claims) -> Value {
    let repositories = f
        .canonical
        .iter()
        .map(|path| RepositoryCandidates {
            repository: path.to_str().unwrap().into(),
            commits: f.tips.clone(),
        })
        .collect();
    serde_json::to_value(f.runtime.host.lock().unwrap().job_have(
        &c.jti,
        repositories,
        vec!["https://example.com/dependency".into()],
    ))
    .unwrap()
}
pub(super) fn reopen(f: &Fixture) {
    let mut host = f.runtime.host.lock().unwrap();
    host.replace_journal(Box::<MemoryJournal>::default());
    let (journal, snapshot) = SqliteJournal::open(&f.root.path().join("host")).unwrap();
    host.snapshot = snapshot;
    host.replace_journal(Box::new(journal));
}
fn dependency(commit: &str) -> MirrorRef {
    MirrorRef {
        repository: None,
        dependency: Some("https://example.com/dependency".into()),
        reference: format!("refs/ck-motor/dep/{commit}"),
        commit: commit.into(),
    }
}

#[tokio::test]
async fn verified_reports_are_disk_account_workspace_and_mirror_scoped() {
    let mut f = Fixture::new();
    let a = active(&mut f);
    assert_eq!(reply(&f, &a)["repositories"][0]["commits"], json!([]));
    let mut refs = f.refs(&f.tips);
    refs.push(dependency(&f.tips[0]));
    f.report(&a.jti, 1, true, refs.clone());
    let held = reply(&f, &a);
    assert_eq!(held["repositories"][0]["commits"], json!([f.tips[0]]));
    assert_eq!(held["git_dependencies"][0]["commits"], json!([f.tips[0]]));

    let mut b = f.claims();
    b.sub = "acct:01ARZ3NDEKTSV4RRFFQ69G5FAW".into();
    let mut c = f.claims();
    c.workspace_key_hash = Some(motor_lease::base64url(&[6; 32]));
    for other in [&b, &c] {
        f.runtime
            .host
            .lock()
            .unwrap()
            .claim(other.clone(), unix_ms())
            .unwrap();
        assert_eq!(reply(&f, other)["repositories"][0]["commits"], json!([]));
        assert_eq!(
            reply(&f, other)["git_dependencies"][0]["commits"],
            json!([])
        );
    }
    assert_ne!(f.disk_id(&a.jti), f.disk_id(&b.jti));
    assert_ne!(f.disk_id(&a.jti), f.disk_id(&c.jti));
    f.report(&a.jti, 2, false, vec![refs[0].clone(), refs[2].clone()]);
    let retired = reply(&f, &a);
    assert_eq!(retired["repositories"][0]["commits"], json!([]));
    assert_eq!(retired["repositories"][1]["commits"], json!([f.tips[1]]));
    assert_eq!(retired["git_dependencies"][0]["commits"], json!([]));
}

#[tokio::test]
async fn report_and_sequence_commit_together_and_invalidation_fences_late_reports_after_restart() {
    let mut f = Fixture::new();
    let c = active(&mut f);
    let refs = f.refs(&f.tips);
    f.report(&c.jti, 10, true, refs.clone());
    reopen(&f);
    assert_eq!(
        reply(&f, &c)["repositories"][0]["commits"],
        json!([f.tips[0]])
    );
    f.report(&c.jti, 20, false, refs.clone());
    reopen(&f);
    f.report(&c.jti, 10, true, refs.clone());
    assert!(
        f.held(&c.jti).is_empty(),
        "an old import cannot undo a retirement"
    );
    f.report(&c.jti, 21, true, refs.clone());
    f.runtime
        .host
        .lock()
        .unwrap()
        .job_transfer_refused(&c.jti)
        .unwrap();
    reopen(&f);
    for sequence in [10, 21, 22] {
        f.report(&c.jti, sequence, true, refs.clone());
    }
    assert!(
        f.held(&c.jti).is_empty(),
        "even an undelivered import from the invalidated VM is fenced"
    );
}

#[tokio::test]
async fn unclean_stops_loss_unknown_identity_and_unknown_targets_clear_before_next_have() {
    for fault in [
        "stop",
        "gone",
        "oom",
        "interrupted",
        "lost_report",
        "identity",
        "target",
    ] {
        let mut f = Fixture::new();
        let c = active(&mut f);
        let refs = f.refs(&f.tips);
        f.report(&c.jti, 1, true, refs.clone());
        match fault {
            "stop" => f
                .runtime
                .host
                .lock()
                .unwrap()
                .job_stop_intent(&c.jti, true)
                .unwrap(),
            // A VMM gone without an exit receipt clears the disk's have-record
            // only once the evidence bound passes; before that the receipt is
            // still expected and decides whether the record is kept.
            "gone" => {
                let first = unix_ms();
                let gone = |at| {
                    f.runtime
                        .host
                        .lock()
                        .unwrap()
                        .job_vmm_gone(&c.jti, at)
                        .unwrap()
                };
                gone(first);
                assert!(!f.held(&c.jti).is_empty(), "gone within the bound");
                gone(first + 10_000);
            }
            "oom" => {
                f.runtime
                    .host
                    .lock()
                    .unwrap()
                    .job_exit_received(
                        &motor_vm_helper::job::ExitReport {
                            job_id: c.jti.clone(),
                            oom_kill: 1,
                            egress_bytes: None,
                        },
                        unix_ms(),
                    )
                    .unwrap();
            }
            "interrupted" => f
                .runtime
                .host
                .lock()
                .unwrap()
                .job_stopped(&c.jti, true)
                .unwrap(),
            _ => {
                let mut report = MirrorRefReport {
                    imported: false,
                    refs: refs.clone(),
                    dropped_reports: 0,
                };
                match fault {
                    "lost_report" => report.dropped_reports = 1,
                    "identity" => report.refs[0].repository = None,
                    "target" => report.refs[0].commit.clear(),
                    _ => unreachable!(),
                }
                let event: TenantEvent = serde_json::from_value(
                    json!({"event_seq":2,"event":"mirror_refs","mirror_refs":report}),
                )
                .unwrap();
                f.runtime
                    .host
                    .lock()
                    .unwrap()
                    .event(&c.jti, &event, unix_ms())
                    .unwrap();
            }
        }
        reopen(&f);
        f.report(&c.jti, 3, true, refs);
        assert!(f.held(&c.jti).is_empty(), "{fault}");
        assert_eq!(
            reply(&f, &c)["repositories"][0]["commits"],
            json!([]),
            "{fault}"
        );
    }
}

#[tokio::test]
async fn bounded_record_drops_oldest_pins_without_inventing_commits() {
    let mut f = Fixture::new();
    let c = active(&mut f);
    let refs: Vec<_> = (0..4100)
        .map(|index| MirrorRef {
            repository: Some(f.canonical[0].to_str().unwrap().into()),
            dependency: None,
            reference: format!("refs/snapshots/{index}"),
            commit: format!("{index:040x}"),
        })
        .collect();
    f.report(&c.jti, 1, true, refs.clone());
    let held = f.held(&c.jti);
    assert_eq!(held.len(), 4096);
    assert_eq!(held.first(), Some(&refs[4]));
    assert_eq!(held.last(), refs.last());
    reopen(&f);
    assert_eq!(f.held(&c.jti), held);
}

struct FailedJournal;
impl Journal for FailedJournal {
    fn store(&mut self, _: &Snapshot) -> std::io::Result<()> {
        Err(std::io::Error::other("injected sync failure"))
    }
}

#[tokio::test]
async fn import_report_and_cursor_are_atomic_across_host_journal_failure_and_restart() {
    for point in ["before_report", "report_sync", "after_report_sync"] {
        let mut f = Fixture::new();
        let c = active(&mut f);
        let event: TenantEvent = serde_json::from_value(json!({"event_seq":1,"event":"mirror_refs","mirror_refs":{"imported":true,"refs":f.refs(&f.tips)}})).unwrap();
        // A crash before receipt, during the journal sync, or after sync must
        // recover the ref pins and their replay cursor as one transaction.
        if point == "report_sync" {
            f.runtime
                .host
                .lock()
                .unwrap()
                .replace_journal(Box::new(FailedJournal));
            assert!(f
                .runtime
                .host
                .lock()
                .unwrap()
                .event(&c.jti, &event, unix_ms())
                .is_err());
        } else if point == "after_report_sync" {
            f.runtime
                .host
                .lock()
                .unwrap()
                .event(&c.jti, &event, unix_ms())
                .unwrap();
        }
        reopen(&f);
        let held = f.held(&c.jti);
        if point == "after_report_sync" {
            assert_eq!(held, f.refs(&f.tips));
            assert_eq!(
                f.runtime.host.lock().unwrap().snapshot.vms[&c.jti].event_seq,
                1
            );
        } else {
            assert!(held.is_empty(), "{point}");
            assert_eq!(
                f.runtime
                    .host
                    .lock()
                    .unwrap()
                    .snapshot
                    .vms
                    .get(&c.jti)
                    .map(|v| v.event_seq)
                    .unwrap_or(0),
                0
            );
        }
    }
}
