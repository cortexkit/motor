use super::exit_tests::{fixture, peer};
use super::*;
use std::sync::{mpsc, Arc};

fn known(response: Response) -> bool {
    serde_json::to_value(response.reply).unwrap()["known"] == true
}

#[test]
fn stop_or_adopt_of_an_id_whose_start_holds_admission_waits_for_its_record() {
    for stop in [false, true] {
        let base = scratch();
        let jobs = Arc::new(fixture(&base));
        let intent = start_record(&jobs, start(), peer().uid, None);
        let id = intent.start.job_id.clone();
        // This thread is Start(X): it holds admission and has not yet made
        // its durable record visible.
        let admission = jobs.admission.lock();
        jobs.start_available(&intent.start, intent.disk_device, intent.disk_inode)
            .unwrap();
        let asking = jobs.clone();
        let asked = id.clone();
        let (sent, reply) = mpsc::channel();
        let request = std::thread::spawn(move || {
            let op = if stop {
                Operation::Stop(asked)
            } else {
                Operation::Adopt(asked)
            };
            sent.send(asking.handle(op, peer())).unwrap();
        });
        let early = reply.recv_timeout(Duration::from_millis(200));
        jobs.launch_record(admission, intent, |_| Ok(())).unwrap();
        let result = match early {
            Ok(result) => result,
            Err(_) => reply.recv_timeout(Duration::from_secs(5)).unwrap(),
        };
        request.join().unwrap();
        match result {
            Ok(response) => assert!(known(response), "stop={stop}: reported absent"),
            // Release needs a root-owned jail; reaching it proves the stop
            // found the started VM rather than reporting it absent.
            Err(error) => {
                assert!(stop && peer().uid != 0, "stop={stop}: {error}");
                assert!(error.0.contains("must be root-owned"), "{error}");
            }
        }
        assert!(!jobs.proven_absent(&id).unwrap());
        std::fs::remove_dir_all(base).unwrap();
    }
}

#[test]
fn proven_absence_refuses_a_later_start_until_its_retention_ends() {
    for stop in [false, true] {
        let base = scratch();
        let jobs = fixture(&base);
        let id = start().job_id;
        let op = if stop {
            Operation::Stop(id.clone())
        } else {
            Operation::Adopt(id.clone())
        };
        assert!(!known(jobs.handle(op, peer()).unwrap()));
        // A Start for X that was queued behind admission arrives afterwards,
        // in this helper or after a restart.
        for jobs in [&jobs, &fixture(&base)] {
            let error = jobs.start(start(), peer()).err().unwrap();
            assert!(error.0.contains("reported absent"), "stop={stop}: {error}");
            assert!(jobs.view(&id).is_none());
            assert!(!jobs.records_path.join(format!("{id}.json")).exists());
        }
        // Absence is not a record: nothing is reserved, reported or adopted.
        assert!(std::fs::read_dir(&jobs.records_path)
            .unwrap()
            .all(|entry| entry.unwrap().path().extension().is_none()));
        assert!(jobs.records().is_empty());
        jobs.maintain_at(SystemTime::now() + TOMBSTONE_RETENTION - Duration::from_secs(60))
            .unwrap();
        assert!(jobs.proven_absent(&id).unwrap());
        let jobs = fixture(&base);
        jobs.maintain_at(SystemTime::now() + TOMBSTONE_RETENTION + Duration::from_secs(1))
            .unwrap();
        assert!(!jobs.proven_absent(&id).unwrap());
        let error = jobs.start(start(), peer()).err().unwrap();
        assert!(!error.0.contains("reported absent"), "{error}");
        std::fs::remove_dir_all(base).unwrap();
    }
}
