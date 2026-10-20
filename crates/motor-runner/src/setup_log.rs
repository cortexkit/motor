use std::{
    cell::{Cell, RefCell},
    io,
    sync::Arc,
    time::Instant,
};
use uuid::Uuid;

thread_local! {
    static JOB: Cell<Option<Uuid>> = const { Cell::new(None) };
    static EVENTS: RefCell<Option<Arc<crate::tenant::EventLog>>> = const { RefCell::new(None) };
}

pub(crate) fn materialize<T>(
    job_id: Uuid,
    cache_fill: bool,
    events: Option<Arc<crate::tenant::EventLog>>,
    setup: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    // Setup is synchronous on its own thread. Keep the identity in that thread
    // so low-level lock helpers can log it without exposing checkout paths.
    struct Reset(Option<Uuid>, Option<Arc<crate::tenant::EventLog>>);
    impl Drop for Reset {
        fn drop(&mut self) {
            JOB.set(self.0);
            EVENTS.replace(self.1.take());
        }
    }
    let _reset = Reset(JOB.replace(Some(job_id)), EVENTS.replace(events));
    let started = Instant::now();
    tracing::info!(%job_id, cache_fill, "materialize started");
    let result = phase("materialize", setup);
    tracing::info!(%job_id, cache_fill, elapsed_ms = started.elapsed().as_millis() as u64,
        success = result.is_ok(), "materialize finished");
    result
}

pub(crate) fn lock_wait<T>(lock: &'static str, acquire: impl FnOnce() -> T) -> T {
    let Some(job_id) = JOB.get() else {
        return acquire();
    };
    let started = Instant::now();
    tracing::info!(%job_id, lock, "setup lock wait started");
    let result = phase(lock, acquire);
    tracing::info!(%job_id, lock, wait_ms = started.elapsed().as_millis() as u64,
        "setup lock wait finished");
    result
}

/// Keep operation names static so diagnostics never forward uploaded text.
pub(crate) fn phase<T>(name: &'static str, operation: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let emit = |finished| {
        if let Some(id) = JOB.get() {
            EVENTS.with_borrow(|events| {
                if let Some(events) = events {
                    let phase = motor_protocol::TenantSetupPhase {
                        phase: name.into(),
                        finished,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    };
                    if let Err(error) = events.setup(id, phase) {
                        tracing::warn!(%id, kind = ?error.kind(), "cannot persist setup phase");
                    }
                }
            });
        }
    };
    emit(false);
    let result = operation();
    emit(true);
    result
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn cargo_skip(lockfile: &std::path::Path, error: &str) {
    tracing::warn!(job_id = ?JOB.get(), lockfile = %lockfile.display(), %error,
        "optional nested Cargo prefetch skipped; job will run offline");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);
    impl io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn materialization_logs_identity_waits_and_failure_without_paths() {
        let logs = Logs::default();
        let subscriber = motor_protocol::logging::LineSubscriber::new("[runner]", logs.clone());
        let id = Uuid::now_v7();
        tracing::subscriber::with_default(subscriber, || {
            let result: io::Result<()> = materialize(id, false, None, || {
                for lock in ["tree_access", "dependency_mirror", "build_wrapper"] {
                    lock_wait(lock, || ());
                }
                Err(io::Error::other("/secret/path"))
            });
            assert!(result.is_err());
            assert_eq!(JOB.get(), None);
            lock_wait("outside_setup", || ());
        });
        let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 8, "{output}");
        assert!(lines
            .iter()
            .all(|line| line.contains(" INFO ") && line.contains(&id.to_string())));
        assert!(lines[0].contains("materialize started"));
        for (index, lock) in ["tree_access", "dependency_mirror", "build_wrapper"]
            .iter()
            .enumerate()
        {
            assert!(lines[index * 2 + 1].contains("setup lock wait started"));
            let finished = lines[index * 2 + 2];
            assert!(finished.contains("setup lock wait finished"));
            assert!(finished.contains(&format!("lock={lock}")));
            assert!(finished.contains("wait_ms="));
        }
        assert!(lines[7].contains("materialize finished"));
        assert!(lines[7].contains("elapsed_ms="));
        assert!(lines[7].contains("success=false"));
        assert!(!output.contains("/secret/path"));
    }

    #[test]
    fn optional_cargo_warning_names_job_lockfile_and_cargo_error() {
        let logs = Logs::default();
        let subscriber = motor_protocol::logging::LineSubscriber::new("[runner]", logs.clone());
        let id = Uuid::now_v7();
        tracing::subscriber::with_default(subscriber, || {
            materialize(id, false, || {
                cargo_skip(
                    std::path::Path::new("probes/helper/Cargo.lock"),
                    "Cargo.lock needs updating: --locked was passed to prevent this",
                );
                Ok(())
            })
            .unwrap();
        });
        let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let warning = output.lines().find(|line| line.contains(" WARN ")).unwrap();
        assert!(warning.contains(&id.to_string()), "{output}");
        assert!(warning.contains("probes/helper/Cargo.lock"), "{output}");
        assert!(
            warning.contains("--locked was passed to prevent this"),
            "{output}"
        );
        assert!(output.contains("success=true"), "{output}");
    }
}

#[cfg(test)]
mod event_tests {
    use super::*;

    #[test]
    fn setup_phases_are_durable_ordered_and_name_the_blocked_operation() {
        let root = cortexkit_test_support::ScratchDir::new("setup-events");
        let events = Arc::new(crate::tenant::EventLog::open(&root).unwrap());
        let id = Uuid::now_v7();
        materialize(id, false, Some(events.clone()), || {
            lock_wait("build_wrapper", || {
                let mut phases = Vec::new();
                while let Some(event) = events.first() {
                    assert_eq!(event.jti, Some(id));
                    let phase = event.setup.unwrap();
                    phases.push((phase.phase, phase.finished));
                    events.ack(event.event_seq).unwrap();
                }
                assert_eq!(
                    phases,
                    [
                        ("materialize".into(), false),
                        ("build_wrapper".into(), false)
                    ]
                );
            });
            Ok(())
        })
        .unwrap();
        drop(events);
        let events = crate::tenant::EventLog::open(&root).unwrap();
        let mut finished = Vec::new();
        while let Some(event) = events.first() {
            assert_eq!(event.event, motor_protocol::TenantEventKind::SetupPhase);
            let phase = event.setup.unwrap();
            assert!(phase.finished);
            finished.push(phase.phase);
            events.ack(event.event_seq).unwrap();
        }
        assert_eq!(finished, ["build_wrapper", "materialize"]);
    }
}
