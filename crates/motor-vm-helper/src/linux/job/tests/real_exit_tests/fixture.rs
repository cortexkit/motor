use super::*;
use crate::linux::tenant_network;
use network::Access;

pub(super) struct PrivateNetwork {
    pub base: PathBuf,
    pub system: network::System,
}

impl Access for PrivateNetwork {
    fn lock(&self) -> Result<tenant_network::lock::Guard> {
        tenant_network::lock::acquire_at(&self.base.join("network-lock"), 0)
    }
    fn tenants(&self) -> Result<Vec<Tap>> {
        Ok(Vec::new())
    }
    fn install(&self, _: &[Tap]) -> Result<()> {
        let _lock = self.lock()?;
        let taps: Vec<_> = network::read_records(&self.base.join("records"))?
            .iter()
            .filter_map(Record::tap)
            .collect();
        // Only the installer is fixture-specific: render real policy from our
        // durable allocations under the network lock, in the private netns.
        tenant_network::test_install_job_policy("public0", &taps)
    }
    fn create_namespace(&self, id: &str) -> Result<PathBuf> {
        self.system.create_namespace(id)
    }
    fn reconcile_namespaces(&self, retained: &BTreeSet<String>) -> Result<()> {
        self.system.reconcile_namespaces(retained)
    }
    fn remove_namespace(&self, id: &str) -> Result<()> {
        self.system.remove_namespace(id)
    }
    fn ensure(&self, id: &str, tap: &Tap) -> Result<()> {
        self.system.ensure(id, tap)
    }
    fn egress_bytes(&self, tap: &Tap) -> Result<Option<u64>> {
        self.system.egress_bytes(tap)
    }
    fn remove(&self, id: &str, tap: &Tap, lock: &tenant_network::lock::Guard) -> Result<()> {
        self.system.remove(id, tap, lock)
    }
}

pub(super) fn artifact(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(name).unwrap_or_else(|| {
        panic!("{name} must name an absolute real-VM artifact path; see the test's prerequisites")
    }));
    assert!(
        path.is_absolute() && path.is_file(),
        "{name}: invalid artifact {path:?}"
    );
    path
}

pub(super) fn command(program: &str, args: &[&str]) {
    let output = Command::new(program).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(super) struct Resources {
    pub jobs: Arc<LinuxJobs>,
    pub base: PathBuf,
    pub slice: String,
    pub releases: Vec<mpsc::Sender<()>>,
    pub boot: Option<std::thread::JoinHandle<Result<Response>>>,
    pub observers: Vec<std::thread::JoinHandle<()>>,
}

impl Resources {
    pub fn cleanup(&mut self) -> Result<()> {
        for release in self.releases.drain(..) {
            // A failed test may already have dropped the receiver.
            let _ = release.send(());
        }
        let mut errors = Vec::new();
        let records = self.jobs.records();
        // Kill all fixture groups before any stop waits on a child. No live
        // helper service uses these scratch-backed job records or test cgroups.
        for id in records.keys() {
            let group = self.jobs.group(id);
            if group.exists() {
                if let Err(error) = std::fs::write(group.join("cgroup.kill"), "1") {
                    errors.push(format!("kill {}: {error}", group.display()));
                }
            }
        }
        if let Some(boot) = self.boot.take() {
            match boot.join() {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => errors.push(format!("boot thread: {error}")),
                Err(_) => errors.push("boot thread panicked".into()),
            }
        }
        for (id, record) in &records {
            let group = self.jobs.group(id);
            if group.exists() {
                if let Err(error) = tenant::wait_empty(&group) {
                    errors.push(format!("wait {}: {error}", group.display()));
                    continue;
                }
            }
            if let Err(error) = self.jobs.stop(id) {
                errors.push(format!("stop {id}: {error}"));
            }
            if group.exists() || namespace::path(id)?.exists() {
                errors.push(format!("job {id} retained its cgroup or namespace"));
            }
            if self
                .jobs
                .view(id)
                .is_some_and(|view| view.record.tap_index.is_some())
            {
                errors.push(format!("job {id} retained its TAP allocation"));
            }
            if let Some(index) = record.tap_index {
                match nft_objects() {
                    Ok(objects) if tap_objects(index).is_disjoint(&objects) => {}
                    Ok(_) => errors.push(format!("job {id} retained its nft objects")),
                    Err(error) => errors.push(format!("inspect job {id} nft objects: {error}")),
                }
            }
        }
        for observer in self.observers.drain(..) {
            if observer.join().is_err() {
                errors.push("exit observer panicked".into());
            }
        }
        if errors.is_empty() && self.jobs.groups.exists() {
            let status = Command::new("systemctl")
                .args(["stop", &self.slice])
                .status()?;
            if !status.success() {
                errors.push(format!("stop test slice {}: {status}", self.slice));
            }
        }
        if !errors.is_empty() {
            return Err(Error(format!(
                "{}; preserving {}",
                errors.join("; "),
                self.base.display()
            )));
        }
        std::fs::remove_dir_all(&self.base)?;
        Ok(())
    }
}

impl Drop for Resources {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            if std::thread::panicking() {
                eprintln!("real VM cleanup failed during unwind: {error}");
            } else {
                panic!("real VM cleanup failed: {error}");
            }
        }
    }
}
