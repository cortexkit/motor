use std::{cell::Cell, io, time::Instant};
use uuid::Uuid;

thread_local! {
    static JOB: Cell<Option<Uuid>> = const { Cell::new(None) };
}

pub(crate) fn materialize<T>(
    job_id: Uuid,
    cache_fill: bool,
    setup: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    // Setup is synchronous on its own thread. Keep the identity in that thread
    // so low-level lock helpers can log it without exposing checkout paths.
    struct Reset(Option<Uuid>);
    impl Drop for Reset {
        fn drop(&mut self) {
            JOB.set(self.0);
        }
    }
    let _reset = Reset(JOB.replace(Some(job_id)));
    let started = Instant::now();
    tracing::info!(%job_id, cache_fill, "materialize started");
    let result = setup();
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
    let result = acquire();
    tracing::info!(%job_id, lock, wait_ms = started.elapsed().as_millis() as u64,
        "setup lock wait finished");
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
            let result: io::Result<()> = materialize(id, false, || {
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
