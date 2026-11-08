use super::*;
use motor_host::claim_log::{record, RefusalCause};
use motor_host::state::{Admission, Error, Refusal};

fn capture(f: impl FnOnce()) -> Vec<String> {
    let logs = super::billing::Logs::default();
    let dispatch = tracing::Dispatch::new(logs.clone());
    // A second live subscriber disables tracing's single-subscriber interest
    // shortcut, which can otherwise suppress callsites first hit by other tests.
    let _spare = tracing::Dispatch::new(super::billing::Logs::default());
    tracing::dispatcher::with_default(&dispatch, || {
        tracing::callsite::rebuild_interest_cache();
        f();
    });
    let lines = logs.0.lock().unwrap();
    lines
        .iter()
        .filter(|line| line.contains("message=run_claim "))
        .cloned()
        .collect()
}

fn assert_record(line: &str, job: &str, mode: &str, result: &str, cause: Option<&str>) {
    assert!(line.contains(&format!("jti={job} ")), "{line}");
    assert!(line.contains(&format!("mode=\"{mode}\" ")), "{line}");
    assert!(line.contains(&format!("result=\"{result}\" ")), "{line}");
    if let Some(cause) = cause {
        assert!(line.contains(&format!("cause=\"{cause}\" ")), "{line}");
    } else {
        assert!(!line.contains("cause="), "{line}");
        assert!(!line.contains("reason="), "{line}");
    }
}

#[test]
fn every_run_claim_logs_result_and_refusal_cause_in_both_modes() {
    for settings in host_settings() {
        let mode = settings.mode().as_str();
        let mut h = host();
        h.settings = settings;
        h.guest_ready("tenant", 1).unwrap();
        let lines = capture(|| {
            h.claim(claims(JOB), NOW * 1000).unwrap();
            assert!(matches!(
                h.claim(claims(JOB), NOW * 1000),
                Err(Error::Refused(Refusal::Auth))
            ));
            let mut full = claims(JOB2);
            full.vcpus = 3;
            assert!(matches!(
                h.claim(full, NOW * 1000),
                Err(Error::Refused(Refusal::HostFull))
            ));
            let mut expired = claims(JOB3);
            expired.exp = NOW;
            assert!(matches!(
                h.claim(expired, NOW * 1000),
                Err(Error::Refused(Refusal::Expired))
            ));
            let mut attach = claims(JOB);
            attach.purpose = Purpose::Attach;
            h.claim(attach, NOW * 1000).unwrap();
        });
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_record(&lines[0], JOB, mode, "admitted", None);
        assert_record(&lines[1], JOB, mode, "refused", Some("auth"));
        assert_record(&lines[2], JOB2, mode, "refused", Some("permits"));
        assert_record(&lines[3], JOB3, mode, "refused", Some("expired"));
        assert!(lines[2].contains("reason=\"host_full\""));
        assert!(!lines.join("\n").contains(&claims(JOB).sub));
        assert!(!lines
            .join("\n")
            .contains(claims(JOB).workspace_key_hash.as_ref().unwrap()));
    }
}

#[test]
fn replacement_waiting_and_storage_refusal_are_logged_in_both_modes() {
    for settings in host_settings() {
        let mode = settings.mode().as_str();
        let mut h = host();
        h.settings = settings;
        h.guest_ready("tenant", 1).unwrap();
        let (id, _) = h.claim(claims(JOB), NOW * 1000).unwrap();
        h.delivered(id).unwrap();
        h.close(id).unwrap();
        let lines = capture(|| {
            let mut replacement = claims(JOB);
            replacement.iat += 1;
            replacement.exp += 1;
            assert_eq!(
                h.claim(replacement, NOW * 1000).unwrap().1,
                Admission::Waiting
            );
            h.enforce_storage();
            assert!(matches!(
                h.claim(claims(JOB2), NOW * 1000),
                Err(Error::Refused(Refusal::HostFull))
            ));
        });
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_record(&lines[0], JOB, mode, "waiting", None);
        assert_record(&lines[1], JOB2, mode, "refused", Some("storage"));
    }
}

#[test]
fn journal_failure_is_a_refused_run_claim_not_an_admission() {
    struct FailedJournal;
    impl Journal for FailedJournal {
        fn store(&mut self, _: &Snapshot) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
        }
    }
    for settings in host_settings() {
        let mode = settings.mode().as_str();
        let mut h = Host::new(tenants(), Snapshot::default(), Box::new(FailedJournal));
        h.settings = settings;
        let lines = capture(|| {
            assert!(matches!(
                h.claim(claims(JOB), NOW * 1000),
                Err(Error::Journal(_))
            ));
        });
        assert_eq!(lines.len(), 1);
        assert_record(&lines[0], JOB, mode, "refused", Some("storage_full"));
        assert!(h.snapshot.leases.is_empty());
    }
}

#[test]
fn quarantine_has_its_own_cause_but_keeps_the_capacity_wire_reason() {
    for settings in host_settings() {
        let mode = settings.mode();
        // The disk lifecycle supplies this cause when it refuses a quarantined
        // workspace; the claim log must distinguish it from permit exhaustion.
        let lines = capture(|| {
            record(mode, &claims(JOB), Err(RefusalCause::QuarantinedWorkspace));
            record(mode, &claims(JOB2), Err(RefusalCause::Permits));
        });
        assert_eq!(lines.len(), 2);
        assert_record(
            &lines[0],
            JOB,
            mode.as_str(),
            "refused",
            Some("quarantined_workspace"),
        );
        assert_record(&lines[1], JOB2, mode.as_str(), "refused", Some("permits"));
        assert!(lines
            .iter()
            .all(|line| line.contains("reason=\"host_full\"")));
    }
}
