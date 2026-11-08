//! Best-effort cgroup working-set telemetry, independent of job execution.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
#[cfg(any(target_os = "linux", test))]
use std::path::Path;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
};

const UNKNOWN: u64 = u64::MAX;
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

#[derive(Default)]
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
struct IntegralState {
    byte_ms: Option<u128>,
    previous: Option<(u64, u64)>,
    incomplete: bool,
}

/// Present only for burst leases. Null is distinct from a measured zero.
#[derive(Clone, Default)]
pub(crate) struct OverIntegral(Arc<Mutex<IntegralState>>);

impl OverIntegral {
    pub(crate) fn present<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Self>, D::Error> {
        Self::deserialize(deserializer).map(Some)
    }

    pub(crate) fn get(&self) -> Option<u64> {
        self.0
            .lock()
            .unwrap()
            .byte_ms
            .map(|value| value.div_ceil(1 << 20).min(u128::from(MAX_SAFE_INTEGER)) as u64)
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn sample(&self, bytes: Option<u64>, stated_mib: u64, elapsed_ms: u64) {
        let mut state = self.0.lock().unwrap();
        let Some(bytes) = bytes else {
            // A missing cgroup before the first read is normal during launch.
            // A later gap cannot be integrated without inventing usage.
            if state.previous.is_some() {
                state.incomplete = true;
                state.byte_ms = None;
            }
            return;
        };
        if state.incomplete {
            return;
        }
        let over = bytes.saturating_sub(stated_mib.saturating_mul(1 << 20));
        let mut total = state.byte_ms.unwrap_or(0);
        if let Some((previous_ms, previous_over)) = state.previous {
            // Hold each observed working set until the next observation. Keep
            // sub-MiB contributions until reporting, rather than rounding every
            // interval up and charging file cache or fractional MiB repeatedly.
            total = total.saturating_add(
                u128::from(previous_over) * u128::from(elapsed_ms.saturating_sub(previous_ms)),
            );
        }
        state.byte_ms = Some(total);
        state.previous = Some((elapsed_ms, over));
    }
}

impl Serialize for OverIntegral {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.get().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OverIntegral {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Option::<u64>::deserialize(deserializer)?;
        Ok(Self(Arc::new(Mutex::new(IntegralState {
            byte_ms: value.map(|value| u128::from(value) << 20),
            ..Default::default()
        }))))
    }
}

#[derive(Default)]
struct State {
    peak_mib: AtomicU64,
    stopped: AtomicBool,
    #[cfg(target_os = "linux")]
    started: AtomicBool,
    logged: AtomicBool,
    thread: OnceLock<thread::Thread>,
}

/// Clones share the live sample without taking the engine or output locks.
/// Its JSON representation is the same optional integer used by lifecycle events.
#[derive(Clone)]
pub(crate) struct WorkingPeak(Arc<State>);

impl Default for WorkingPeak {
    fn default() -> Self {
        Self::from_value(None)
    }
}

impl WorkingPeak {
    fn from_value(value: Option<u64>) -> Self {
        Self(Arc::new(State {
            peak_mib: AtomicU64::new(value.unwrap_or(UNKNOWN)),
            ..State::default()
        }))
    }

    pub(crate) fn get(&self) -> Option<u64> {
        match self.0.peak_mib.load(Ordering::Relaxed) {
            UNKNOWN => None,
            value => Some(value),
        }
    }

    pub(crate) fn is_unknown(&self) -> bool {
        self.get().is_none()
    }

    pub(crate) fn log_once(&self) -> bool {
        !self.0.logged.swap(true, Ordering::Relaxed)
    }

    pub(crate) fn stop(&self) {
        self.0.stopped.store(true, Ordering::Relaxed);
        if let Some(thread) = self.0.thread.get() {
            thread.unpark();
        }
    }

    #[cfg(any(target_os = "linux", test))]
    fn sample(&self, path: &Path) -> Option<u64> {
        let bytes = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| working_bytes(&text))?;
        let mib = bytes.div_ceil(1 << 20);
        let _ = self
            .0
            .peak_mib
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                Some(if old == UNKNOWN { mib } else { old.max(mib) })
            });
        Some(bytes)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn start(
        &self,
        unit: &str,
        stated_mib: Option<u64>,
        over: Option<OverIntegral>,
        publish: impl Fn(Option<u64>, Option<Option<u64>>) + Send + 'static,
    ) {
        if self.0.started.swap(true, Ordering::Relaxed) {
            return;
        }
        // All Linux job launchers use JOB_MEMORY_PROPERTIES, whose slice fixes
        // this path. No systemctl subprocess or process walk is needed to sample.
        let path = Path::new("/sys/fs/cgroup/motor.slice/motor-jobs.slice")
            .join(unit)
            .join("memory.stat");
        let weak = Arc::downgrade(&self.0);
        let started = std::time::Instant::now();
        let _ = thread::Builder::new()
            .name("job-working-memory".into())
            .spawn(move || loop {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                let _ = state.thread.set(thread::current());
                let peak = WorkingPeak(state);
                // The stop wakeup makes one final cheap read best-effort. Never
                // join this thread: terminal publication must not wait for it.
                let bytes = peak.sample(&path);
                let stopped = peak.0.stopped.load(Ordering::Relaxed);
                if let Some(over) = over.as_ref().filter(|_| bytes.is_some() || !stopped) {
                    over.sample(
                        bytes,
                        stated_mib.unwrap_or(0),
                        started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    );
                }
                publish(peak.get(), over.as_ref().map(OverIntegral::get));
                if stopped {
                    return;
                }
                drop(peak);
                thread::park_timeout(std::time::Duration::from_secs(1));
            });
    }
}

impl Serialize for WorkingPeak {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.get().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WorkingPeak {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<u64>::deserialize(deserializer).map(Self::from_value)
    }
}

/// `kernel` includes its subcategories on newer kernels; adding those again
/// would double count. Older kernels use only the unreclaimable components.
#[cfg(any(target_os = "linux", test))]
fn working_bytes(text: &str) -> Option<u64> {
    let value = |key: &str| {
        text.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == key).then(|| fields.next()?.parse::<u64>().ok())
        })?
    };
    let kernel = if text
        .lines()
        .any(|line| line.split_whitespace().next() == Some("kernel"))
    {
        value("kernel")?
    } else {
        value("kernel_stack")?
            .checked_add(value("pagetables")?)?
            .checked_add(value("percpu")?)?
            .checked_add(value("slab_unreclaimable")?)?
    };
    value("anon")?
        .checked_add(value("shmem")?)?
        .checked_add(kernel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overage_integrates_only_working_bytes_above_stated_without_interval_rounding() {
        let peak = WorkingPeak::default();
        let over = OverIntegral::default();
        let root = cortexkit_test_support::scratch_root()
            .join(format!("overage-memory-{}", motor_protocol::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("memory.stat");
        for (elapsed, anon, shmem, kernel, expected) in [
            (0, 1 << 20, 0, 0, 0),
            (100, 1 << 20, 1 << 18, 1 << 18, 0),
            (101, 1 << 20, 1 << 18, 1 << 18, 1),
            (102, 0, 0, 0, 1),
            (1102, 0, 0, 0, 1),
        ] {
            std::fs::write(&path, format!("anon {anon}\nshmem {shmem}\nkernel {kernel}\nfile 9999999999\nslab_reclaimable 9999999999\n")).unwrap();
            over.sample(peak.sample(&path), 1, elapsed);
            assert_eq!(over.get(), Some(expected));
        }
        assert_eq!(peak.get(), Some(2));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn overage_distinguishes_unmeasured_zero_gaps_and_saturates_safe_integers() {
        let over = OverIntegral::default();
        assert_eq!(over.get(), None);
        over.sample(None, 1, 0);
        over.sample(Some(1 << 20), 1, 100);
        assert_eq!(over.get(), Some(0));
        over.sample(Some(3 << 20), 1, 200);
        over.sample(Some(1 << 20), 1, 450);
        assert_eq!(over.get(), Some(500));
        over.sample(None, 1, 500);
        assert_eq!(over.get(), None);
        over.sample(Some(3 << 20), 1, 1000);
        assert_eq!(
            over.get(),
            None,
            "a later read cannot repair an attribution gap"
        );
        let over = OverIntegral::default();
        over.sample(Some(u64::MAX), 0, 0);
        over.sample(Some(u64::MAX), 0, u64::MAX);
        assert_eq!(over.get(), Some(MAX_SAFE_INTEGER));
        assert_eq!(serde_json::to_value(&over).unwrap(), MAX_SAFE_INTEGER);
    }

    #[test]
    fn memory_stat_kernel_variants_exclude_file_cache_and_do_not_double_count() {
        let old = "anon 1048576\nshmem 524288\nkernel_stack 16384\npagetables 32768\npercpu 4096\nslab_unreclaimable 8192\nfile 9999999999\nslab_reclaimable 9999999999\n";
        assert_eq!(working_bytes(old), Some(1_634_304));
        assert_eq!(
            working_bytes(&format!("{old}kernel 1048576\n")),
            Some(2_621_440)
        );
        assert_eq!(working_bytes("anon 0\nshmem 0\nkernel 0\n"), Some(0));
        assert_eq!(working_bytes("anon 1\nshmem 0\n"), None);
        assert_eq!(working_bytes("anon 1\nshmem 0\nkernel invalid\n"), None);
        assert_eq!(
            working_bytes(&format!("anon {}\nshmem 1\nkernel 1\n", u64::MAX)),
            None
        );
    }

    #[test]
    fn memory_stat_seam_tracks_peak_rounds_up_and_retains_last_good_read() {
        let root = cortexkit_test_support::scratch_root()
            .join(format!("working-memory-{}", motor_protocol::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("memory.stat");
        let peak = WorkingPeak::default();
        peak.sample(&path);
        assert_eq!(peak.get(), None);
        std::fs::write(&path, "anon 0\nshmem 0\nkernel_stack 0\npagetables 0\npercpu 0\nslab_unreclaimable 1\nfile 9999999999\n").unwrap();
        peak.sample(&path);
        assert_eq!(peak.get(), Some(1));
        let peak = WorkingPeak::default();
        for (bytes, expected) in [(0, 0), (1, 1), (1 << 20, 1), ((1 << 20) + 1, 2), (0, 2)] {
            std::fs::write(
                &path,
                format!("anon {bytes}\nshmem 0\nkernel 0\nfile 9999999999\n"),
            )
            .unwrap();
            peak.sample(&path);
            assert_eq!(peak.clone().get(), Some(expected));
        }
        std::fs::write(&path, "anon broken").unwrap();
        peak.sample(&path);
        assert_eq!(peak.get(), Some(2));
        let json = serde_json::to_string(&peak).unwrap();
        assert_eq!(json, "2");
        assert_eq!(
            serde_json::from_str::<WorkingPeak>(&json).unwrap().get(),
            Some(2)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
