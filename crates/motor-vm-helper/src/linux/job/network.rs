use super::*;
use crate::linux::tenant_network::{self, Tap};

pub(super) trait Access: Send {
    fn tenants(&self) -> Result<Vec<Tap>>;
    fn install(&mut self, taps: &[Tap]) -> Result<()>;
    fn create_namespace(&mut self, id: &str) -> Result<PathBuf>;
    fn reconcile_namespaces(&mut self, retained: &BTreeSet<String>) -> Result<()>;
    fn remove_namespace(&mut self, id: &str) -> Result<()>;
    fn ensure(&mut self, id: &str, tap: &Tap) -> Result<()>;
    fn egress_bytes(&mut self, tap: &Tap) -> Result<Option<u64>>;
    fn remove(&mut self, id: &str, tap: &Tap) -> Result<()>;
}

pub(super) struct System(pub Config);

impl Access for System {
    fn tenants(&self) -> Result<Vec<Tap>> {
        tenant_taps(Path::new(crate::tenant::REGISTRY))
    }
    fn install(&mut self, taps: &[Tap]) -> Result<()> {
        tenant_network::validate_taps(taps)?;
        tenant_network::install_taps(taps, &self.0)?.require_ready()
    }
    fn create_namespace(&mut self, id: &str) -> Result<PathBuf> {
        namespace::create(id)
    }
    fn reconcile_namespaces(&mut self, retained: &BTreeSet<String>) -> Result<()> {
        namespace::reconcile(retained)
    }
    fn remove_namespace(&mut self, id: &str) -> Result<()> {
        namespace::remove(id)
    }
    fn ensure(&mut self, id: &str, tap: &Tap) -> Result<()> {
        namespace::connect(id, tap)
    }
    fn egress_bytes(&mut self, tap: &Tap) -> Result<Option<u64>> {
        tenant_network::optional_egress_bytes(tap)
    }
    fn remove(&mut self, id: &str, tap: &Tap) -> Result<()> {
        namespace::disconnect(id, tap)?;
        tenant_network::remove_allocated(tap)?;
        // The host owns the NAT mapping; the job namespace tracks forwarding
        // too. Retire both before the durable allocation can be reused.
        super::conntrack::flush(tap.tap_index)
    }
}

fn tenant_taps(root: &Path) -> Result<Vec<Tap>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut taps = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| Error("invalid tenant registry name".into()))?;
        let registry: crate::tenant::Registry =
            serde_json::from_reader(fs::trusted_file(&path)?)
                .map_err(|error| Error(format!("invalid tenant registry: {error}")))?;
        registry.validate(name)?;
        taps.push(Tap::from(&registry));
    }
    tenant_network::validate_taps(&taps)?;
    Ok(taps)
}

pub(super) fn read_records(root: &Path) -> Result<Vec<Record>> {
    if !root.try_exists()? {
        return Ok(Vec::new());
    }
    fs::trusted(root, &std::fs::symlink_metadata(root)?)?;
    let mut records = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let record: Record = serde_json::from_reader(fs::trusted_file(&path)?)
            .map_err(|error| Error(format!("invalid job record: {error}")))?;
        record.start.validate()?;
        if path.file_stem().and_then(|s| s.to_str()) != Some(&record.start.job_id) {
            return Err(Error("job record identity mismatch".into()));
        }
        records.push(record);
    }
    Ok(records)
}

pub(super) fn check_capacity(job_count: u32, tenants: usize) -> Result<()> {
    if u64::from(job_count) + tenants as u64 > 254 {
        return Err(Error(format!(
            "job_vms.uid_count {job_count} + registered tenants {tenants} exceeds TAP range 254"
        )));
    }
    Ok(())
}

pub(super) fn allocate(tenants: &[Tap], records: &BTreeMap<String, Record>) -> Result<u8> {
    let mut taps = tenants.to_vec();
    taps.extend(records.values().filter_map(Record::tap));
    tenant_network::validate_taps(&taps)?;
    (1..=254)
        .find(|index| taps.iter().all(|tap| tap.tap_index != *index))
        .ok_or_else(|| Error("no free job TAP index in range 1..254".into()))
}
