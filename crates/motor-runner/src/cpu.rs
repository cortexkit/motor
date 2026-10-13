#[cfg(any(target_os = "linux", test))]
use std::path::Path;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

/// Online sibling groups, sorted by CPU number. On hosts with enough cores the
/// first core is daemon-only (see `RESERVE_MIN_CORES`).
#[derive(Clone)]
pub(crate) struct CpuTopology {
    cores: Vec<Vec<u32>>,
    reserved: usize,
}

/// The daemon's setup work gets one physical core to itself only where giving
/// it up still leaves room for jobs. On a small host (a 2-vCPU CI runner has a
/// single core) reserving it would leave nothing allocatable and refuse every
/// job, so the daemon shares the CPUs there instead.
const RESERVE_MIN_CORES: usize = 4;

#[cfg(any(target_os = "linux", test))]
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
        Ok(Self { cores, reserved })
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
    pub(crate) fn place<'a>(
        &self,
        count: u32,
        claims: impl Iterator<Item = &'a [u32]>,
    ) -> Vec<u32> {
        assert!(count > 0 && count <= self.capacity());
        let mut loads: BTreeMap<_, u32> = self
            .cores
            .iter()
            .skip(self.reserved)
            .flatten()
            .map(|cpu| (*cpu, 0))
            .collect();
        for claim in claims {
            for cpu in claim {
                if let Some(load) = loads.get_mut(cpu) {
                    *load += 1;
                }
            }
        }
        let pairs: Vec<_> = self
            .cores
            .iter()
            .skip(self.reserved)
            .flat_map(|core| core.chunks(2))
            .collect();
        let mut selected = BTreeSet::new();
        // Use whole free pairs first. Releases can leave only free half-pairs;
        // avoiding contention matters more than locality in that case.
        for pair in &pairs {
            if selected.len() + 2 <= count as usize
                && pair.len() == 2
                && pair.iter().all(|cpu| loads[cpu] == 0)
            {
                selected.extend(pair.iter().copied());
            }
        }
        while selected.len() < count as usize {
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
                        !(count % 2 == 1 && split),
                        sibling.map(|s| loads[s]).unwrap_or(0),
                        **cpu,
                    )
                })
                .unwrap();
            selected.insert(cpu);
        }
        selected.into_iter().collect()
    }
}

#[cfg(test)]
#[path = "cpu_tests.rs"]
mod tests;
