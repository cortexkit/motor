use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Online sibling groups, sorted by CPU number. On hosts with enough cores the
/// first core is daemon-only (see `RESERVE_MIN_CORES`).
#[derive(Clone)]
pub(crate) struct CpuTopology {
    cores: Vec<Vec<u32>>,
    reserved: usize,
    windows: Option<Arc<WindowsReservation>>,
}

type ActivityProbe = Arc<dyn Fn() -> io::Result<bool> + Send + Sync>;

pub(crate) struct CpuPlacement {
    pub(crate) cpus: Vec<u32>,
    pub(crate) windows_active: Option<bool>,
    pub(crate) avoided_cpus: BTreeSet<u32>,
}

struct WindowsReservation {
    cpus: BTreeSet<u32>,
    slot_lock: PathBuf,
    probe: ActivityProbe,
    last_warning: Mutex<Option<Instant>>,
}

impl WindowsReservation {
    fn warning_due(&self, now: Instant) -> bool {
        let mut last = self.last_warning.lock().unwrap();
        if last.is_some_and(|last| now.duration_since(last) < Duration::from_secs(60)) {
            return false;
        }
        *last = Some(now);
        true
    }

    fn warn_fail_open(&self, error: impl std::fmt::Display) {
        if self.warning_due(Instant::now()) {
            tracing::warn!(%error, slot_lock = %self.slot_lock.display(),
                "Windows CPU avoidance unavailable; placing without exclusions");
        }
    }
}

#[cfg(target_os = "linux")]
fn windows_slot_active(slot_lock: &Path) -> io::Result<bool> {
    slot_active_from(slot_lock, Path::new("/proc/locks"))
}

#[cfg(not(target_os = "linux"))]
fn windows_slot_active(_slot_lock: &Path) -> io::Result<bool> {
    Err(io::Error::other("Windows slot observation requires Linux"))
}

#[cfg(target_os = "linux")]
fn slot_active_from(slot_lock: &Path, locks: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    // Stat on every observation: replacing slot.lock must not leave a cached inode.
    let metadata = match std::fs::metadata(slot_lock) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(io::Error::other("Windows slot lock is not a regular file"));
    }
    flock_is_held(
        &std::fs::read_to_string(locks)?,
        u64::from(libc::major(metadata.dev())),
        u64::from(libc::minor(metadata.dev())),
        metadata.ino(),
    )
}

#[cfg(any(target_os = "linux", test))]
fn flock_is_held(locks: &str, major: u64, minor: u64, inode: u64) -> io::Result<bool> {
    let malformed = || io::Error::other("malformed /proc/locks entry");
    let mut held = false;
    for line in locks.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let blocked = fields.get(1) == Some(&"->");
        let offset = usize::from(blocked);
        if fields.len() != 8 + offset
            || fields[0]
                .strip_suffix(':')
                .and_then(|id| id.parse::<u64>().ok())
                .is_none()
            || fields[4 + offset].parse::<i64>().is_err()
        {
            return Err(malformed());
        }
        let device: Vec<_> = fields[5 + offset].split(':').collect();
        if device.len() != 3 {
            return Err(malformed());
        }
        let entry_major = u64::from_str_radix(device[0], 16).map_err(|_| malformed())?;
        let entry_minor = u64::from_str_radix(device[1], 16).map_err(|_| malformed())?;
        let entry_inode = device[2].parse::<u64>().map_err(|_| malformed())?;
        // Waiting flocks and other lock families do not own the QEMU slot.
        held |= !blocked
            && fields[1] == "FLOCK"
            && fields[3] == "WRITE"
            && (entry_major, entry_minor, entry_inode) == (major, minor, inode);
    }
    Ok(held)
}

/// The daemon's setup work gets one physical core to itself only where giving
/// it up still leaves room for jobs. On a small host (a 2-vCPU CI runner has a
/// single core) reserving it would leave nothing allocatable and refuse every
/// job, so the daemon shares the CPUs there instead.
const RESERVE_MIN_CORES: usize = 4;

pub(crate) fn parse_list(list: &str) -> io::Result<Vec<u32>> {
    let mut cpus = BTreeSet::new();
    for part in list.trim().split(',') {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let first = first
            .parse::<u32>()
            .map_err(|_| io::Error::other("invalid CPU list"))?;
        let last = last
            .parse::<u32>()
            .map_err(|_| io::Error::other("invalid CPU list"))?;
        if first > last {
            return Err(io::Error::other("reversed CPU range"));
        }
        cpus.extend(first..=last);
    }
    Ok(cpus.into_iter().collect())
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn format_list(cpus: &[u32]) -> String {
    cpus.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

impl CpuTopology {
    pub(crate) fn without_daemon_reservation(mut self) -> Self {
        self.reserved = 0;
        self
    }

    /// A VM uses its dedicated CPU set; none of that set is daemon-only.
    pub(crate) fn fixed(cpus: Vec<u32>) -> io::Result<Self> {
        let mut topology = Self::new(vec![cpus])?;
        topology.reserved = 0;
        Ok(topology)
    }
    pub(crate) fn read() -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            Self::from_sysfs(Path::new("/sys/devices/system/cpu"))
        }
        #[cfg(not(target_os = "linux"))]
        {
            // The development host has no sysfs or pinning; model its online CPUs.
            let count = std::thread::available_parallelism()?.get() as u32;
            Self::new((0..count).map(|cpu| vec![cpu]).collect())
        }
    }

    #[cfg(any(target_os = "linux", test))]
    fn from_sysfs(root: &Path) -> io::Result<Self> {
        let online = parse_list(&std::fs::read_to_string(root.join("online"))?)?;
        let mut cores = BTreeSet::new();
        for cpu in &online {
            let siblings = parse_list(&std::fs::read_to_string(
                root.join(format!("cpu{cpu}/topology/thread_siblings_list")),
            )?)?
            .into_iter()
            .filter(|sibling| online.binary_search(sibling).is_ok())
            .collect::<Vec<_>>();
            if !siblings.contains(cpu) {
                return Err(io::Error::other("CPU missing from its sibling group"));
            }
            cores.insert(siblings);
        }
        Self::new(cores.into_iter().collect())
    }

    pub(crate) fn new(mut cores: Vec<Vec<u32>>) -> io::Result<Self> {
        let mut seen = BTreeSet::new();
        for core in &mut cores {
            core.sort_unstable();
            if core.is_empty() || core.iter().any(|cpu| !seen.insert(*cpu)) {
                return Err(io::Error::other("empty or overlapping CPU sibling groups"));
            }
        }
        cores.sort();
        if cores.is_empty() {
            return Err(io::Error::other("no online CPUs"));
        }
        let reserved = usize::from(cores.len() >= RESERVE_MIN_CORES);
        Ok(Self {
            cores,
            reserved,
            windows: None,
        })
    }

    pub(crate) fn avoid_windows(mut self, cpus: &[u32], slot_lock: PathBuf) -> io::Result<Self> {
        if cpus.is_empty()
            || !slot_lock.is_absolute()
            || cpus
                .iter()
                .any(|cpu| !self.cores.iter().any(|core| core.contains(cpu)))
        {
            return Err(io::Error::other(
                "Windows CPU avoidance requires online CPUs and an absolute slot-lock path",
            ));
        }
        let excluded = self
            .cores
            .iter()
            .filter(|core| core.iter().any(|cpu| cpus.contains(cpu)))
            .flatten()
            .copied()
            .collect();
        let path = slot_lock.clone();
        self.windows = Some(Arc::new(WindowsReservation {
            cpus: excluded,
            slot_lock,
            probe: Arc::new(move || windows_slot_active(&path)),
            last_warning: Mutex::new(None),
        }));
        Ok(self)
    }

    #[cfg(test)]
    pub(crate) fn set_windows_probe(&mut self, probe: ActivityProbe) {
        Arc::get_mut(self.windows.as_mut().unwrap()).unwrap().probe = probe;
    }

    pub(crate) fn capacity(&self) -> u32 {
        self.cores
            .iter()
            .skip(self.reserved)
            .map(Vec::len)
            .sum::<usize>() as u32
    }

    /// Records are the reservation ledger: setup persists a set before doing work,
    /// terminal releases it, and reattachment retains it without a second counter.
    pub(crate) fn place_with_status<'a>(
        &self,
        count: u32,
        claims: impl Iterator<Item = &'a [u32]>,
    ) -> CpuPlacement {
        assert!(count > 0 && count <= self.capacity());
        let windows_active = self
            .windows
            .as_ref()
            .and_then(|windows| match (windows.probe)() {
                Ok(active) => Some(active),
                Err(error) => {
                    windows.warn_fail_open(error);
                    None
                }
            });
        let mut excluded = BTreeSet::new();
        if windows_active == Some(true) {
            let windows = self.windows.as_ref().unwrap();
            excluded.clone_from(&windows.cpus);
            if self
                .cores
                .iter()
                .skip(self.reserved)
                .flatten()
                .all(|cpu| excluded.contains(cpu))
            {
                windows.warn_fail_open("Windows CPUs cover every allocatable CPU");
                excluded.clear();
            }
        }
        let cores: Vec<_> = self
            .cores
            .iter()
            .skip(self.reserved)
            .filter(|core| !core.iter().any(|cpu| excluded.contains(cpu)))
            .collect();
        // Only new placements use this filtered view; recorded claims stay put.
        let mut loads: BTreeMap<_, u32> = cores
            .iter()
            .flat_map(|core| core.iter())
            .map(|cpu| (*cpu, 0))
            .collect();
        // A cpuset cannot repeat a CPU. Large claims share the remaining set;
        // admission and resource quotas still use the original requested count.
        let placement_count = (count as usize).min(loads.len());
        for claim in claims {
            for cpu in claim {
                if let Some(load) = loads.get_mut(cpu) {
                    *load += 1;
                }
            }
        }
        let pairs: Vec<_> = cores.iter().flat_map(|core| core.chunks(2)).collect();
        let mut selected = BTreeSet::new();
        // Use whole free pairs first. Releases can leave only free half-pairs;
        // avoiding contention matters more than locality in that case.
        for pair in &pairs {
            if selected.len() + 2 <= placement_count
                && pair.len() == 2
                && pair.iter().all(|cpu| loads[cpu] == 0)
            {
                selected.extend(pair.iter().copied());
            }
        }
        while selected.len() < placement_count {
            let cpu = *loads
                .keys()
                .filter(|cpu| !selected.contains(cpu))
                .min_by_key(|cpu| {
                    let sibling = pairs
                        .iter()
                        .find(|pair| pair.contains(cpu))
                        .and_then(|pair| pair.iter().find(|other| *other != *cpu));
                    let paired = sibling.is_some_and(|sibling| selected.contains(sibling));
                    let split = sibling.is_some_and(|sibling| loads[sibling] != loads[*cpu]);
                    // For an odd tail, fill an already-split pair before splitting another.
                    (
                        loads[*cpu],
                        !paired,
                        !(placement_count % 2 == 1 && split),
                        sibling.map(|s| loads[s]).unwrap_or(0),
                        **cpu,
                    )
                })
                .unwrap();
            selected.insert(cpu);
        }
        CpuPlacement {
            cpus: selected.into_iter().collect(),
            windows_active,
            avoided_cpus: excluded,
        }
    }

    #[cfg(test)]
    pub(crate) fn place<'a>(
        &self,
        count: u32,
        claims: impl Iterator<Item = &'a [u32]>,
    ) -> Vec<u32> {
        self.place_with_status(count, claims).cpus
    }
}

#[cfg(test)]
#[path = "cpu_tests.rs"]
mod tests;
