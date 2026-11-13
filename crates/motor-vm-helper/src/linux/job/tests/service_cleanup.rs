use super::*;

// The outer test owns this guard, not either helper incarnation: the VMM must
// survive a helper exit but must never survive the outer test's scope.
pub(super) struct ServiceResources {
    base: PathBuf,
    group: PathBuf,
    cleanup: Box<dyn Cleanup>,
}

trait Cleanup {
    fn stop_helper(&self) -> Result<()>;
    fn kill_group(&self, group: &Path) -> Result<bool>;
    fn wait_empty(&self, group: &Path) -> Result<()>;
    fn remove_group(&self, group: &Path) -> Result<()>;
    fn stop_slice(&self) -> Result<()>;
    fn remove_scratch(&self, base: &Path) -> Result<()>;
}

impl ServiceResources {
    pub(super) fn new(base: PathBuf, group: PathBuf) -> Self {
        Self {
            base,
            group,
            cleanup: Box::new(System),
        }
    }

    fn cleanup_group(&self) -> Result<()> {
        if self.cleanup.kill_group(&self.group)? {
            self.cleanup.wait_empty(&self.group)?;
            self.cleanup.remove_group(&self.group)?;
        }
        Ok(())
    }

    fn cleanup(&self) -> Result<()> {
        let mut errors = Vec::new();
        // Fence spawning before inspecting the cgroup. Otherwise a helper that
        // is still starting can create the VMM after an early cleanup check.
        if let Err(error) = self.cleanup.stop_helper() {
            errors.push(error.0);
        }
        // Still attempt the kill if stopping the service failed. A failed
        // emptiness proof must not be followed by cgroup or scratch deletion.
        if let Err(error) = self.cleanup_group() {
            errors.push(error.0);
        }
        if let Err(error) = self.cleanup.stop_slice() {
            errors.push(error.0);
        }
        if errors.is_empty() {
            self.cleanup.remove_scratch(&self.base)
        } else {
            Err(Error(errors.join("; ")))
        }
    }
}

impl Drop for ServiceResources {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            let message = format!(
                "service fixture cleanup failed for {}: {error}; scratch retained at {}",
                self.group.display(),
                self.base.display()
            );
            if std::thread::panicking() {
                // Preserve the original assertion failure without a second
                // panic aborting the process and hiding the cleanup diagnostic.
                eprintln!("{message}");
            } else {
                panic!("{message}");
            }
        }
    }
}

struct System;

fn stop_unit(unit: &str) -> Result<()> {
    let output = Command::new("systemctl").args(["stop", unit]).output()?;
    // Transient --collect units may already have disappeared after a helper
    // incarnation finishes. systemctl uses exit code 5 for a unit not loaded.
    if output.status.success() || output.status.code() == Some(5) {
        return Ok(());
    }
    Err(Error(format!(
        "stop {unit}: {}: {}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )))
}

impl Cleanup for System {
    fn stop_helper(&self) -> Result<()> {
        stop_unit("motor-vm-helper-job-test.service")
    }
    fn kill_group(&self, group: &Path) -> Result<bool> {
        match std::fs::write(group.join("cgroup.kill"), "1") {
            Ok(()) => Ok(true),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && !group.try_exists()? =>
            {
                Ok(false)
            }
            Err(error) => Err(Error(format!("kill {}: {error}", group.display()))),
        }
    }
    fn wait_empty(&self, group: &Path) -> Result<()> {
        tenant::wait_empty(group)
    }
    fn remove_group(&self, group: &Path) -> Result<()> {
        match std::fs::remove_dir(group) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error(format!("remove {}: {error}", group.display()))),
        }
    }
    fn stop_slice(&self) -> Result<()> {
        stop_unit("motorvmtest.slice")
    }
    fn remove_scratch(&self, base: &Path) -> Result<()> {
        match std::fs::remove_dir_all(base) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Clone)]
struct Fake {
    events: Arc<std::sync::Mutex<Vec<&'static str>>>,
    fail: Option<&'static str>,
    group_exists: bool,
}

impl Fake {
    fn event(&self, event: &'static str) -> Result<()> {
        self.events.lock().unwrap().push(event);
        if self.fail == Some(event) {
            return Err(Error(format!("injected {event} failure")));
        }
        Ok(())
    }
}

impl Cleanup for Fake {
    fn stop_helper(&self) -> Result<()> {
        self.event("stop-helper")
    }
    fn kill_group(&self, _: &Path) -> Result<bool> {
        self.event("kill")?;
        Ok(self.group_exists)
    }
    fn wait_empty(&self, _: &Path) -> Result<()> {
        self.event("wait-empty")
    }
    fn remove_group(&self, _: &Path) -> Result<()> {
        self.event("remove-group")
    }
    fn stop_slice(&self) -> Result<()> {
        self.event("stop-slice")
    }
    fn remove_scratch(&self, _: &Path) -> Result<()> {
        self.event("remove-scratch")
    }
}

fn fixture(
    root: &Path,
    fail: Option<&'static str>,
    group_exists: bool,
) -> (ServiceResources, Fake) {
    let fake = Fake {
        events: Arc::default(),
        fail,
        group_exists,
    };
    let resources = ServiceResources {
        base: root.join("scratch"),
        group: root.join("job-group"),
        cleanup: Box::new(fake.clone()),
    };
    (resources, fake)
}

fn panic_text(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap()
}

#[test]
fn cleanup_fences_helper_before_kill_wait_remove_on_success_and_panic() {
    let root = cortexkit_test_support::ScratchDir::new("service-cleanup");
    for unwind in [false, true] {
        let (resources, fake) = fixture(&root, None, true);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _resources = resources;
            assert!(!unwind, "original fixture panic");
        }));
        assert_eq!(result.is_err(), unwind);
        if let Err(panic) = result {
            assert_eq!(panic_text(panic.as_ref()), "original fixture panic");
        }
        assert_eq!(
            *fake.events.lock().unwrap(),
            [
                "stop-helper",
                "kill",
                "wait-empty",
                "remove-group",
                "stop-slice",
                "remove-scratch"
            ]
        );
    }
}

#[test]
fn cleanup_failure_is_visible_and_keeps_scratch_without_removing_a_live_group() {
    let root = cortexkit_test_support::ScratchDir::new("service-cleanup-failure");
    for phase in [
        "stop-helper",
        "kill",
        "wait-empty",
        "remove-group",
        "stop-slice",
    ] {
        let (resources, fake) = fixture(&root, Some(phase), true);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(resources)))
            .expect_err("cleanup failure was silently discarded");
        let text = panic_text(panic.as_ref());
        assert!(
            text.contains(&format!("injected {phase} failure")),
            "{text}"
        );
        let events = fake.events.lock().unwrap();
        assert_eq!(events.first(), Some(&"stop-helper"));
        assert!(events.contains(&"kill"));
        assert!(events.contains(&"stop-slice"));
        assert!(!events.contains(&"remove-scratch"));
        if phase == "kill" || phase == "wait-empty" {
            assert!(!events.contains(&"remove-group"));
        }
    }
}

#[test]
fn cleanup_failure_during_panic_keeps_the_original_panic_and_scratch() {
    let root = cortexkit_test_support::ScratchDir::new("service-cleanup-unwind");
    let (resources, fake) = fixture(&root, Some("wait-empty"), true);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _resources = resources;
        panic!("original fixture panic");
    }))
    .unwrap_err();
    assert_eq!(panic_text(panic.as_ref()), "original fixture panic");
    assert_eq!(
        *fake.events.lock().unwrap(),
        ["stop-helper", "kill", "wait-empty", "stop-slice"]
    );
}

#[test]
fn cleanup_accepts_a_group_already_removed_by_reconciliation() {
    let root = cortexkit_test_support::ScratchDir::new("service-cleanup-absent");
    let (resources, fake) = fixture(&root, None, false);
    drop(resources);
    assert_eq!(
        *fake.events.lock().unwrap(),
        ["stop-helper", "kill", "stop-slice", "remove-scratch"]
    );
}
