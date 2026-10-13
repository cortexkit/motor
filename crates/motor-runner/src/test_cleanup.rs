use crate::platform::ProcessIdentity;
use std::sync::{Arc, Mutex};

type UnitCommand = dyn Fn(&str, &str) + Send + Sync;

pub(crate) struct Processes {
    closed: bool,
    quiet: bool,
    processes: Vec<ProcessIdentity>,
    quiesce: Vec<Box<dyn FnOnce() -> bool + Send>>,
    unit_command: Arc<UnitCommand>,
}
impl Default for Processes {
    fn default() -> Self {
        Self {
            closed: false,
            quiet: true,
            processes: Vec::new(),
            quiesce: Vec::new(),
            unit_command: Arc::new(systemctl),
        }
    }
}
#[derive(Default)]
pub(crate) struct Cleanup(pub Arc<Mutex<Processes>>, Vec<std::path::PathBuf>);
pub(crate) struct LaunchGuard(pub ProcessIdentity);
impl Drop for LaunchGuard {
    fn drop(&mut self) {
        stop(self.0);
    }
}
impl Processes {
    pub(crate) fn register(&mut self, process: ProcessIdentity) {
        if self.closed {
            self.cleanup(process);
        } else if !self.processes.iter().any(|p| match process.unit {
            Some(unit) => p.unit == Some(unit),
            None => p.unit.is_none() && p.pid == process.pid && p.start == process.start,
        }) {
            self.processes.push(process);
        }
    }
    pub(crate) fn quiesce(&mut self, wait: impl FnOnce() -> bool + Send + 'static) {
        assert!(!self.closed, "runner opened after fixture cleanup");
        self.quiesce.push(Box::new(wait));
    }
    fn cleanup(&self, process: ProcessIdentity) {
        stop_with(process, &*self.unit_command);
        if let Some(id) = process.unit {
            // Result remains available to assertions until fixture teardown.
            (self.unit_command)("reset-failed", &crate::backend::host_unit_name(id));
        }
    }
}
fn systemctl(operation: &str, unit: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("/usr/bin/sudo")
            .args(["-n", "/usr/bin/systemctl", operation, unit])
            .status();
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (operation, unit);
}
pub(crate) fn stop(process: ProcessIdentity) {
    stop_with(process, &systemctl);
}
fn stop_with(process: ProcessIdentity, unit_command: &UnitCommand) {
    if let Some(unit) = process.unit {
        // Synchronous stop ensures the unit is gone before fixture files vanish.
        unit_command("stop", &crate::backend::host_unit_name(unit));
        return;
    }
    if process.pid <= 0 {
        return;
    }
    #[cfg(target_os = "linux")]
    if !crate::platform::identity(process.pid).is_some_and(|p| p.start == process.start) {
        return;
    }
    unsafe {
        libc::kill(-process.pid, libc::SIGKILL);
    }
}
type Captures = std::collections::BTreeMap<std::path::PathBuf, Arc<Mutex<Processes>>>;
static UNIT_CAPTURES: std::sync::OnceLock<Mutex<Captures>> = std::sync::OnceLock::new();

pub(crate) fn processes_for(path: &std::path::Path) -> Option<Arc<Mutex<Processes>>> {
    UNIT_CAPTURES
        .get()?
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find_map(|(root, processes)| path.starts_with(root).then(|| processes.clone()))
}

impl Cleanup {
    pub(crate) fn capture(&mut self, root: &std::path::Path) {
        let mut captures = UNIT_CAPTURES.get_or_init(Mutex::default).lock().unwrap();
        assert!(captures
            .insert(root.to_path_buf(), self.0.clone())
            .is_none());
        self.1.push(root.to_path_buf());
    }
    pub(crate) fn finish(&mut self) -> bool {
        // Launches can still register while their runner finishes its warm fill.
        // Never hold the registry lock while waiting for those launch threads.
        let waits = std::mem::take(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()).quiesce);
        let mut quiet = true;
        for wait in waits {
            quiet &= wait();
        }
        let mut processes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        processes.quiet &= quiet;
        processes.closed = true;
        for process in std::mem::take(&mut processes.processes) {
            processes.cleanup(process);
        }
        processes.quiet
    }
}
pub(crate) fn record_unit(id: uuid::Uuid, cwd: &std::path::Path) {
    let Some(captures) = UNIT_CAPTURES.get() else {
        return;
    };
    let registries: Vec<_> = captures
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(root, _)| cwd.starts_with(root))
        .map(|(_, processes)| processes.clone())
        .collect();
    for registry in registries {
        registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .register(ProcessIdentity {
                pid: 0,
                start: 0,
                unit: Some(id),
            });
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        if !self.finish() {
            // A stuck setup can launch after teardown returns. Keep its closed
            // registry reachable so that late unit is immediately stopped/reset.
            return;
        }
        if let Some(captures) = UNIT_CAPTURES.get() {
            let mut captures = captures.lock().unwrap_or_else(|e| e.into_inner());
            for root in &self.1 {
                captures.remove(root);
            }
        }
    }
}

#[test]
fn fixture_backstop_stops_then_resets_recorded_units_even_on_panic() {
    let commands = Arc::new(Mutex::new(Vec::new()));
    let trace = commands.clone();
    let id = uuid::Uuid::now_v7();
    let scratch = cortexkit_test_support::ScratchDir::new("unit-cleanup");
    let result = std::panic::catch_unwind(|| {
        let mut cleanup = Cleanup::default();
        cleanup.0.lock().unwrap().unit_command = Arc::new(move |operation, unit| {
            trace
                .lock()
                .unwrap()
                .push((operation.to_owned(), unit.to_owned()));
        });
        cleanup.capture(&scratch);
        record_unit(id, &scratch.join("writer"));
        record_unit(id, &scratch.join("writer"));
        assert!(
            commands.lock().unwrap().is_empty(),
            "Result must survive until teardown"
        );
        panic!("deliberate inner test failure");
    });
    assert!(result.is_err());
    let unit = format!("motor-job-{id}.service");
    assert_eq!(
        *commands.lock().unwrap(),
        vec![
            ("stop".to_owned(), unit.clone()),
            ("reset-failed".to_owned(), unit)
        ]
    );
}

#[test]
fn fixture_backstop_covers_a_unit_registered_after_cleanup() {
    let mut cleanup = Cleanup::default();
    let registry = cleanup.0.clone();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let trace = commands.clone();
    registry.lock().unwrap().unit_command = Arc::new(move |operation, unit| {
        trace
            .lock()
            .unwrap()
            .push((operation.to_owned(), unit.to_owned()));
    });
    assert!(cleanup.finish());
    let id = uuid::Uuid::now_v7();
    registry.lock().unwrap().register(ProcessIdentity {
        pid: 0,
        start: 0,
        unit: Some(id),
    });
    let unit = format!("motor-job-{id}.service");
    assert_eq!(
        *commands.lock().unwrap(),
        vec![
            ("stop".to_owned(), unit.clone()),
            ("reset-failed".to_owned(), unit)
        ]
    );
}

#[test]
fn stuck_fixture_keeps_a_closed_registry_for_late_launches() {
    let scratch = cortexkit_test_support::ScratchDir::new("stuck-unit-cleanup");
    let mut cleanup = Cleanup::default();
    cleanup.capture(&scratch);
    let commands = Arc::new(Mutex::new(Vec::new()));
    let trace = commands.clone();
    cleanup.0.lock().unwrap().unit_command = Arc::new(move |operation, unit| {
        trace
            .lock()
            .unwrap()
            .push((operation.to_owned(), unit.to_owned()));
    });
    cleanup.0.lock().unwrap().quiesce(|| false);
    assert!(!cleanup.finish());
    assert!(!cleanup.finish());
    drop(cleanup);
    let id = uuid::Uuid::now_v7();
    record_unit(id, &scratch.join("late-writer"));
    UNIT_CAPTURES
        .get()
        .unwrap()
        .lock()
        .unwrap()
        .remove(scratch.path());
    let unit = format!("motor-job-{id}.service");
    assert_eq!(
        *commands.lock().unwrap(),
        vec![
            ("stop".to_owned(), unit.clone()),
            ("reset-failed".to_owned(), unit)
        ]
    );
}

#[test]
fn guarded_helper_reaps_output() {
    let output = crate::test_process::ChildGuard::spawn(
        std::process::Command::new("/bin/sh")
            .args(["-c", "printf guarded"])
            .stdout(std::process::Stdio::piped()),
    )
    .unwrap()
    .wait_with_output()
    .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"guarded");
}

#[test]
fn panicking_helper_scope_kills_its_process_group() {
    use std::{
        os::unix::process::CommandExt,
        process::Command,
        time::{Duration, Instant},
    };
    let mut child = Command::new("sleep")
        .arg("300")
        .process_group(0)
        .spawn()
        .unwrap();
    let group = child.id() as i32;
    let result = std::panic::catch_unwind(|| {
        let _guard = crate::test_process::GroupGuard(group);
        panic!("deliberate inner test failure");
    });
    assert!(result.is_err());
    let deadline = Instant::now() + Duration::from_secs(2);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::park_timeout(Duration::from_millis(10));
    }
    let gone = unsafe { libc::kill(-group, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    // Even a failed cleanup assertion must not leak the deliberately held helper.
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
    let _ = child.wait();
    assert!(gone, "helper process group survived a panicking inner test");
}

type CheckoutEvents = Mutex<Vec<(&'static str, std::thread::ThreadId)>>;
static CHECKOUT_WORK: std::sync::OnceLock<
    Mutex<std::collections::BTreeMap<std::path::PathBuf, std::sync::Weak<CheckoutEvents>>>,
> = std::sync::OnceLock::new();

/// Capture actual calls for one fixture's paths across all threads. Other
/// fixtures use different roots, so parallel tests cannot contaminate the trace.
pub(crate) struct CheckoutWorkCapture {
    root: std::path::PathBuf,
    thread: std::thread::ThreadId,
    events: Arc<CheckoutEvents>,
}
impl CheckoutWorkCapture {
    pub(crate) fn begin(root: &std::path::Path) -> Self {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut captures = CHECKOUT_WORK.get_or_init(Mutex::default).lock().unwrap();
        assert!(!captures.contains_key(root));
        captures.insert(root.to_path_buf(), Arc::downgrade(&events));
        Self {
            root: root.to_path_buf(),
            thread: std::thread::current().id(),
            events,
        }
    }
    pub(crate) fn events(&self) -> Vec<&'static str> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|(operation, _)| *operation)
            .collect()
    }
    pub(crate) fn startup_thread_events(&self) -> Vec<&'static str> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, thread)| *thread == self.thread)
            .map(|(operation, _)| *operation)
            .collect()
    }
}
impl Drop for CheckoutWorkCapture {
    fn drop(&mut self) {
        CHECKOUT_WORK
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&self.root);
    }
}
pub(crate) fn record_checkout_work(operation: &'static str, path: &std::path::Path) {
    let Some(captures) = CHECKOUT_WORK.get() else {
        return;
    };
    let events: Vec<_> = captures
        .lock()
        .unwrap()
        .iter()
        .filter(|(root, _)| path.starts_with(root))
        .filter_map(|(_, events)| events.upgrade())
        .collect();
    for events in events {
        events
            .lock()
            .unwrap()
            .push((operation, std::thread::current().id()));
    }
}
